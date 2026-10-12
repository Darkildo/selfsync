//! Проверка целостности хранилища (`selfsync verify`): базы, хэши блобов, блобы, на
//! которые ссылаются записи, запись ключа шифрования. С `--repair-from` битые и
//! отсутствующие блобы берутся из бэкапа; что не восстановлено, просится у устройств
//! (`wanted_blobs`): битый блоб уходит в `quarantine/`, и устройство с целым файлом
//! заливает его заново при следующем синке.
//!
//! Имя блоба — хэш хранимых байтов (для зашифрованных тоже), поэтому ключ не нужен.
//! Работает на живом сервере: из блобов меняются только битые (уходят в карантин),
//! `uploads/` не трогается; блоб, появившийся во время обхода, проверится в следующий
//! раз.

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use selfsync_core::hash::{Hash, Hasher};
use selfsync_core::path::canonical_decode;

use crate::blobs::BlobStore;
use crate::config::Config;
use crate::db::vault::{meta_blob, set_meta_int, unwant_blob, want_blobs, wanted_blobs};
use crate::gc::{self, GcOptions};
use crate::state::open_vault;

/// Сколько путей показывать для отсутствующего блоба.
const MAX_REFS: usize = 5;

#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyOptions<'a> {
    /// Каталог бэкапа (`selfsync backup`) или каталог vault'а из него.
    pub repair_from: Option<&'a Path>,
    /// Прогресс в stderr раз в секунду.
    pub progress: bool,
}

/// Блоб, на который ссылаются записи, а файла нет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub hash: Hash,
    /// Пути (открытые — как есть, зашифрованные — пометкой), не больше [`MAX_REFS`].
    pub paths: Vec<String>,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    /// Ответ `PRAGMA integrity_check` базы vault'а, если не «ok».
    pub db_errors: Vec<String>,
    pub blobs: u64,
    pub bytes: u64,
    /// Содержимое не совпадает с именем (и не восстановлено).
    pub corrupt: Vec<Hash>,
    /// Нет файла (и не восстановлен).
    pub missing: Vec<Missing>,
    /// Восстановлены из бэкапа.
    pub repaired: Vec<Hash>,
    /// Не восстановлены: хэш и причина.
    pub unrepaired: Vec<(Hash, &'static str)>,
    /// Из плана `gc` (без чистки истории): блобы без ссылок и брошенные загрузки.
    pub orphans: u64,
    pub orphan_bytes: u64,
    pub uploads: u64,
    /// Запись ключа шифрования отсутствует или не разбирается.
    pub key_problem: Option<String>,
    /// Просятся у устройств после проверки: хэш и с какого времени (мс).
    pub wanted: Vec<(Hash, i64)>,
    pub elapsed: Duration,
}

impl VerifyReport {
    pub fn clean(&self) -> bool {
        self.db_errors.is_empty()
            && self.corrupt.is_empty()
            && self.missing.is_empty()
            && self.key_problem.is_none()
    }
}

/// `PRAGMA integrity_check`: пусто, если база цела.
pub fn integrity(c: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut st = c.prepare("PRAGMA integrity_check")?;
    let rows: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(if rows.len() == 1 && rows[0] == "ok" {
        Vec::new()
    } else {
        rows
    })
}

/// SHA-256 файла и его размер.
fn hash_file(path: &Path) -> io::Result<(Hash, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((h.finish(), total))
}

fn show_path(bytes: &[u8]) -> String {
    match canonical_decode(bytes) {
        Ok(p) if p.encrypted => "(зашифрованный путь)".to_owned(),
        Ok(p) => p
            .segments
            .iter()
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => "(путь не разбирается)".to_owned(),
    }
}

fn referenced(c: &Connection) -> rusqlite::Result<HashSet<Vec<u8>>> {
    let mut out = HashSet::new();
    for sql in [
        "SELECT DISTINCT hash FROM files WHERE hash IS NOT NULL",
        "SELECT DISTINCT hash FROM revisions WHERE hash IS NOT NULL",
    ] {
        let mut st = c.prepare(sql)?;
        for h in st.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
            out.insert(h?);
        }
    }
    Ok(out)
}

fn paths_of(c: &Connection, h: &Hash) -> rusqlite::Result<Vec<String>> {
    let mut st = c.prepare(
        "SELECT path FROM files WHERE hash = ?1 UNION SELECT path FROM revisions WHERE hash = ?1
         LIMIT ?2",
    )?;
    let rows = st.query_map(
        params![h.to_vec(), i64::try_from(MAX_REFS).unwrap_or(5)],
        |r| r.get::<_, Vec<u8>>(0),
    )?;
    rows.map(|p| p.map(|p| show_path(&p))).collect()
}

fn check_key(c: &Connection) -> rusqlite::Result<Option<String>> {
    match meta_blob(c, "vault_key")? {
        Some(rec) => Ok(selfsync_core::crypto::parse_record(&rec)
            .err()
            .map(|e| format!("запись ключа шифрования не разбирается: {e}"))),
        None => {
            let encrypted: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE substr(path, 1, 1) = x'01')",
                [],
                |r| r.get(0),
            )?;
            Ok(encrypted
                .then(|| "есть зашифрованные пути, но нет записи ключа шифрования".to_owned()))
        }
    }
}

/// Прогресс обхода раз в секунду.
struct Progress {
    on: bool,
    last: Instant,
}

impl Progress {
    fn tick(&mut self, name: &str, done: usize, total: usize, bytes: u64) {
        if self.on && self.last.elapsed() >= Duration::from_secs(1) {
            self.last = Instant::now();
            eprintln!(
                "vault {name}: {done}/{total} блобов, {}",
                crate::cmd::fmt_bytes(bytes)
            );
        }
    }
}

/// Проверка одного vault'а (и лечение, если задан бэкап).
pub fn verify_vault(
    config: &Config,
    name: &str,
    opts: VerifyOptions<'_>,
) -> anyhow::Result<VerifyReport> {
    let start = Instant::now();
    let v = open_vault(config, name, 1)?;
    let c = v.pool.get()?;
    let mut r = VerifyReport {
        db_errors: integrity(&c)?,
        ..VerifyReport::default()
    };

    // Битые: пересчитать хэш каждого файла в blobs/.
    let list = v.blobs.list()?;
    let mut present = HashSet::with_capacity(list.len());
    let mut progress = Progress {
        on: opts.progress,
        last: Instant::now(),
    };
    for (i, (h, _, _)) in list.iter().enumerate() {
        progress.tick(name, i, list.len(), r.bytes);
        match hash_file(&v.blobs.path_of(h)) {
            Ok((got, size)) => {
                r.blobs += 1;
                r.bytes += size;
                present.insert(h.to_vec());
                if got != *h {
                    r.corrupt.push(*h);
                }
            }
            // Удалён gc или sweep во время обхода.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    // Отсутствующие: ссылки без файла. Блоб мог появиться после листинга — проверить ещё раз.
    let refs = referenced(&c)?;
    let mut missing: Vec<Hash> = refs
        .iter()
        .filter(|&h| !present.contains(h))
        .cloned()
        .filter_map(|h| Hash::from_slice(&h))
        .filter(|h| !v.blobs.exists(h))
        .collect();
    missing.sort_by_key(Hash::to_hex);
    r.corrupt.sort_by_key(Hash::to_hex);

    r.key_problem = check_key(&c)?;

    // Осиротевшие и брошенные — числа из плана gc, история не учитывается.
    let plan = gc::plan(
        &c,
        &v.blobs,
        GcOptions {
            keep_revisions: u64::MAX,
            keep_days: u64::MAX,
        },
        crate::db::now_ms(),
    )?;
    r.orphans = plan.blobs.len() as u64;
    r.orphan_bytes = plan.blobs.iter().map(|b| b.1).sum();
    r.uploads = (plan.uploads.len() + plan.stray_files.len()) as u64;

    if let Some(dir) = opts.repair_from {
        let sources = [dir.join("vaults").join(name), dir.to_path_buf()];
        let mut still = Vec::new();
        for h in std::mem::take(&mut r.corrupt) {
            match repair(&v.blobs, &sources, &h, true) {
                Ok(()) => r.repaired.push(h),
                Err(why) => {
                    r.unrepaired.push((h, why));
                    still.push(h);
                }
            }
        }
        r.corrupt = still;
        let mut still = Vec::new();
        for h in missing {
            match repair(&v.blobs, &sources, &h, false) {
                Ok(()) => r.repaired.push(h),
                Err(why) => {
                    r.unrepaired.push((h, why));
                    still.push(h);
                }
            }
        }
        missing = still;
    }

    // Что не восстановлено — просить у устройств. Битый блоб — в карантин: его байты
    // бесполезны, а на месте он не дал бы залить целый (блоб с таким именем «есть»).
    let mut want = missing.clone();
    for h in &r.corrupt {
        match quarantine(&v.blobs, h) {
            Ok(()) => want.push(*h),
            Err(e) => {
                tracing::warn!(blob = %h.to_hex(), error = %e, "битый блоб не убран в карантин")
            }
        }
    }
    let now = crate::db::now_ms();
    want_blobs(&c, &want, now)?;
    // Запрос больше не нужен: блоб на месте (залит или восстановлен) или ссылок нет.
    for (h, _) in wanted_blobs(&c, usize::MAX)? {
        if !refs.contains(&h.to_vec()) || (v.blobs.exists(&h) && !r.corrupt.contains(&h)) {
            unwant_blob(&c, &h)?;
        }
    }
    r.wanted = wanted_blobs(&c, usize::MAX)?;

    for h in missing {
        let paths = paths_of(&c, &h)?;
        r.missing.push(Missing { hash: h, paths });
    }
    r.elapsed = start.elapsed();
    save_result(&c, &r)?;
    Ok(r)
}

/// Итог проверки в базе vault'а — для `/metrics`.
fn save_result(c: &Connection, r: &VerifyReport) -> rusqlite::Result<()> {
    let n = |x: usize| i64::try_from(x).unwrap_or(i64::MAX);
    set_meta_int(c, "verify_at", crate::db::now_ms())?;
    set_meta_int(c, "verify_corrupt", n(r.corrupt.len()))?;
    set_meta_int(c, "verify_missing", n(r.missing.len()))?;
    set_meta_int(c, "verify_db_errors", n(r.db_errors.len()))?;
    set_meta_int(c, "verify_key_problem", i64::from(r.key_problem.is_some()))
}

/// Битый блоб — в `quarantine/` (`gc` его не трогает).
fn quarantine(store: &BlobStore, h: &Hash) -> io::Result<()> {
    let src = store.path_of(h);
    if !src.is_file() {
        return Ok(());
    }
    let q = store.quarantine_dir();
    std::fs::create_dir_all(&q)?;
    std::fs::rename(&src, q.join(h.to_hex()))
}

/// Взять блоб из бэкапа: скопировать во временный файл, проверить хэш, битый
/// оригинал — в `quarantine/`, копию — на место.
fn repair(
    store: &BlobStore,
    sources: &[PathBuf],
    h: &Hash,
    corrupt: bool,
) -> Result<(), &'static str> {
    let hex = h.to_hex();
    let Some(src) = sources
        .iter()
        .map(|d| d.join("blobs").join(&hex[..2]).join(&hex))
        .find(|p| p.is_file())
    else {
        return Err("нет в бэкапе");
    };
    let attempt = || -> io::Result<bool> {
        let tmp = store.temp_path()?;
        // Копия, не жёсткая ссылка: файл в хранилище не должен делить inode с бэкапом.
        std::fs::copy(&src, &tmp)?;
        if hash_file(&tmp)?.0 != *h {
            let _ = std::fs::remove_file(&tmp);
            return Ok(false);
        }
        let dst = store.path_of(h);
        if corrupt && dst.is_file() {
            let q = store.quarantine_dir();
            std::fs::create_dir_all(&q)?;
            std::fs::rename(&dst, q.join(&hex))?;
        }
        store.finalize(&tmp, h)?;
        Ok(true)
    };
    match attempt() {
        Ok(true) => Ok(()),
        Ok(false) => Err("в бэкапе тоже повреждён"),
        Err(e) => {
            tracing::warn!(blob = %hex, error = %e, "восстановление из бэкапа не удалось");
            Err("ошибка ввода-вывода при восстановлении")
        }
    }
}
