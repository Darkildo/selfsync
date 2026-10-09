//! Сборка мусора: блобы без ссылок, брошенные загрузки, история глубже N ревизий и
//! старше M дней, tombstone'ы старше окна хранения. По умолчанию только план.
//!
//! Безопасность против гонок с записью: блоб удаляется только внутри
//! `BEGIN IMMEDIATE` после повторной проверки ссылок, а `Put` проверяет наличие блоба
//! в такой же транзакции. Свежие блобы (моложе часа) не трогаются: клиент мог
//! загрузить блоб и ещё не успеть отправить операцию, которая на него сошлётся.

use std::collections::HashSet;

use rusqlite::{Connection, TransactionBehavior, params};
use selfsync_core::hash::Hash;

use crate::blobs::{BlobStore, mtime_ms};
use crate::db::vault::{DAY_MS, retention_days};
use crate::db::{i, u};

/// Блоб моложе этого возраста не удаляется.
pub const BLOB_MIN_AGE_MS: i64 = 60 * 60 * 1000;
/// Загрузка без активности дольше суток считается брошенной.
pub const UPLOAD_MAX_AGE_MS: i64 = DAY_MS;

#[derive(Debug, Clone, Copy)]
pub struct GcOptions {
    pub keep_revisions: u64,
    pub keep_days: u64,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            keep_revisions: 20,
            keep_days: 30,
        }
    }
}

#[derive(Debug, Default)]
pub struct GcPlan {
    /// (путь, ревизия)
    pub revisions: Vec<(Vec<u8>, u64)>,
    /// tombstone'ы, которые стираются целиком (с историей)
    pub tombstones: Vec<(Vec<u8>, u64)>,
    /// (хэш, размер)
    pub blobs: Vec<(Hash, u64)>,
    /// (id, размер)
    pub uploads: Vec<(String, u64)>,
    /// осиротевшие временные файлы в uploads/
    pub stray_files: Vec<(String, u64)>,
}

impl GcPlan {
    pub fn bytes(&self) -> u64 {
        self.blobs.iter().map(|b| b.1).sum::<u64>()
            + self.uploads.iter().map(|b| b.1).sum::<u64>()
            + self.stray_files.iter().map(|b| b.1).sum::<u64>()
    }

    pub fn is_empty(&self) -> bool {
        self.revisions.is_empty()
            && self.tombstones.is_empty()
            && self.blobs.is_empty()
            && self.uploads.is_empty()
            && self.stray_files.is_empty()
    }
}

pub fn plan(
    c: &Connection,
    store: &BlobStore,
    opts: GcOptions,
    now: i64,
) -> anyhow::Result<GcPlan> {
    let mut plan = GcPlan::default();
    let keep = opts.keep_revisions.max(1);
    let age_cutoff = now
        - i64::try_from(opts.keep_days)
            .unwrap_or(i64::MAX / DAY_MS)
            .saturating_mul(DAY_MS);
    let retention_cutoff = now - i64::from(retention_days(c)?) * DAY_MS;
    let rename_cutoff = retention_cutoff.min(age_cutoff);

    // tombstone'ы целиком
    {
        let mut st = c.prepare(
            "SELECT path, seq, renamed_to IS NOT NULL FROM files WHERE deleted = 1 AND deleted_at IS NOT NULL
             AND ((renamed_to IS NULL AND deleted_at < ?1) OR (renamed_to IS NOT NULL AND deleted_at < ?2))",
        )?;
        let rows = st.query_map(params![retention_cutoff, rename_cutoff], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, u(r.get(1)?)))
        })?;
        for r in rows {
            plan.tombstones.push(r?);
        }
    }
    let tomb_paths: HashSet<&[u8]> = plan.tombstones.iter().map(|t| t.0.as_slice()).collect();

    // глубокая и старая история
    {
        let mut st =
            c.prepare("SELECT path, rev, created_at FROM revisions ORDER BY path, rev DESC")?;
        let mut rows = st.query([])?;
        let mut cur: Option<Vec<u8>> = None;
        let mut depth = 0u64;
        while let Some(r) = rows.next()? {
            let path: Vec<u8> = r.get(0)?;
            let rev = u(r.get(1)?);
            let created: i64 = r.get(2)?;
            if cur.as_deref() != Some(&path[..]) {
                cur = Some(path.clone());
                depth = 0;
            }
            depth += 1;
            if tomb_paths.contains(&path[..]) {
                continue;
            }
            if depth > keep && created < age_cutoff {
                plan.revisions.push((path, rev));
            }
        }
    }

    // ссылки после чистки
    let pruned: HashSet<(&[u8], u64)> = plan
        .revisions
        .iter()
        .map(|(p, r)| (p.as_slice(), *r))
        .collect();
    let mut referenced: HashSet<Vec<u8>> = HashSet::new();
    {
        let mut st = c.prepare("SELECT hash FROM files WHERE hash IS NOT NULL")?;
        for h in st.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
            referenced.insert(h?);
        }
        let mut st = c.prepare("SELECT path, rev, hash FROM revisions WHERE hash IS NOT NULL")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let path: Vec<u8> = r.get(0)?;
            let rev = u(r.get(1)?);
            if tomb_paths.contains(&path[..]) || pruned.contains(&(&path[..], rev)) {
                continue;
            }
            referenced.insert(r.get(2)?);
        }
    }
    for (h, size, mtime) in store.list()? {
        if !referenced.contains(&h.to_vec()) && now - mtime > BLOB_MIN_AGE_MS {
            plan.blobs.push((h, size));
        }
    }

    // брошенные загрузки
    let mut known = HashSet::new();
    {
        let mut st = c.prepare("SELECT id, updated_at FROM uploads")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for r in rows {
            let (id, updated) = r?;
            if now - updated > UPLOAD_MAX_AGE_MS {
                let size = std::fs::metadata(store.upload_path(&id))
                    .map(|m| m.len())
                    .unwrap_or(0);
                plan.uploads.push((id.clone(), size));
            }
            known.insert(id);
        }
    }
    if let Ok(rd) = std::fs::read_dir(store.uploads_dir()) {
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if known.contains(&name) {
                continue;
            }
            if let Ok(md) = e.metadata()
                && now - mtime_ms(&md) > UPLOAD_MAX_AGE_MS
            {
                plan.stray_files.push((name, md.len()));
            }
        }
    }
    Ok(plan)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub revisions: u64,
    pub tombstones: u64,
    pub blobs: u64,
    pub uploads: u64,
}

pub fn execute(c: &mut Connection, store: &BlobStore, plan: &GcPlan) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut max_seq = 0u64;
        for (path, seq) in &plan.tombstones {
            // Перепроверка: путь мог ожить после построения плана.
            let still: i64 = tx.query_row(
                "SELECT COUNT(*) FROM files WHERE path = ?1 AND deleted = 1",
                params![path],
                |r| r.get(0),
            )?;
            if still == 0 {
                continue;
            }
            tx.execute("DELETE FROM revisions WHERE path = ?1", params![path])?;
            tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
            max_seq = max_seq.max(*seq);
            report.tombstones += 1;
        }
        for (path, rev) in &plan.revisions {
            // Текущая ревизия файла не удаляется никогда.
            report.revisions += u64::try_from(tx.execute(
                "DELETE FROM revisions WHERE path = ?1 AND rev = ?2
                 AND rev < COALESCE((SELECT rev FROM files WHERE path = ?1), ?2 + 1)",
                params![path, i(*rev)],
            )?)
            .unwrap_or(0);
        }
        if max_seq > 0 {
            let cur = u(crate::db::vault::meta_int(&tx, "purged_seq")?.unwrap_or(0));
            if max_seq > cur {
                crate::db::vault::set_meta_int(&tx, "purged_seq", i(max_seq))?;
            }
        }
        tx.commit()?;
    }
    let hashes: Vec<Vec<u8>> = plan.blobs.iter().map(|b| b.0.to_vec()).collect();
    report.blobs = remove_unreferenced(c, store, &hashes, Some(BLOB_MIN_AGE_MS))?;
    for (id, _) in &plan.uploads {
        let _ = std::fs::remove_file(store.upload_path(id));
        c.execute("DELETE FROM uploads WHERE id = ?1", params![id])?;
        report.uploads += 1;
    }
    for (name, _) in &plan.stray_files {
        let _ = std::fs::remove_file(store.uploads_dir().join(name));
    }
    Ok(report)
}

/// Удаляет блобы из списка, на которые больше никто не ссылается. Проверка и
/// удаление — под блокировкой записи, так что `Put` не сошлётся на удаляемый блоб.
pub fn remove_unreferenced(
    c: &mut Connection,
    store: &BlobStore,
    hashes: &[Vec<u8>],
    min_age_ms: Option<i64>,
) -> rusqlite::Result<u64> {
    let mut removed = 0;
    let now = crate::db::now_ms();
    for chunk in hashes.chunks(256) {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for h in chunk {
            let Some(hh) = Hash::from_slice(h) else {
                continue;
            };
            if crate::db::vault::blob_referenced(&tx, h)? {
                continue;
            }
            if let Some(age) = min_age_ms {
                let fresh = std::fs::metadata(store.path_of(&hh))
                    .map(|m| now - mtime_ms(&m) <= age)
                    .unwrap_or(false);
                if fresh {
                    continue;
                }
            }
            if store.remove(&hh).unwrap_or(false) {
                removed += 1;
            }
        }
        tx.commit()?;
    }
    Ok(removed)
}
