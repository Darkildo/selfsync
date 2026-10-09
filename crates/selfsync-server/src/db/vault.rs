//! `meta.db` одного vault'а: файлы, ревизии, монотонный `seq`, настройки, запись ключа.
//!
//! Сервер — упорядоченный лог и хранилище байтов. Он обнаруживает конфликты, но не
//! разрешает их, и ничего не удаляет по своей инициативе, кроме sweeper'а и `gc`.

use prost::Message;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use selfsync_core::hash::Hash;
use selfsync_core::path::{PathError, canonical_decode, canonical_encode, validate_proto_path};
use selfsync_proto::v1 as pb;

use super::{i, migrate, u};

/// Максимум операций в одном `OpsRequest`.
pub const MAX_OPS: usize = 1000;
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

const MIGRATIONS: &[&str] = &["
CREATE TABLE files (
  path         BLOB PRIMARY KEY,
  rev          INTEGER NOT NULL,
  seq          INTEGER NOT NULL,
  hash         BLOB,
  size         INTEGER NOT NULL,
  mtime        INTEGER NOT NULL,
  deleted      INTEGER NOT NULL DEFAULT 0,
  deleted_at   INTEGER,
  folder       INTEGER NOT NULL DEFAULT 0,
  renamed_from BLOB,
  renamed_to   BLOB,
  updated_by   INTEGER NOT NULL
) WITHOUT ROWID;
CREATE UNIQUE INDEX files_seq ON files(seq);
CREATE INDEX files_hash ON files(hash) WHERE hash IS NOT NULL;
CREATE INDEX files_deleted ON files(deleted_at) WHERE deleted = 1;

CREATE TABLE revisions (
  path         BLOB NOT NULL,
  rev          INTEGER NOT NULL,
  hash         BLOB,
  size         INTEGER NOT NULL,
  mtime        INTEGER NOT NULL,
  seq          INTEGER NOT NULL,
  updated_by   INTEGER NOT NULL,
  deleted      INTEGER NOT NULL DEFAULT 0,
  folder       INTEGER NOT NULL DEFAULT 0,
  renamed_from BLOB,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (path, rev)
) WITHOUT ROWID;
CREATE INDEX revisions_hash ON revisions(hash) WHERE hash IS NOT NULL;

CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);

CREATE TABLE uploads (
  id          TEXT PRIMARY KEY,
  hash        BLOB NOT NULL,
  size        INTEGER NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE INDEX uploads_hash ON uploads(hash);
"];

pub fn migrate_vault(c: &mut Connection) -> rusqlite::Result<()> {
    migrate(c, MIGRATIONS)
}

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

pub fn meta_int(c: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
}

pub fn meta_blob(c: &Connection, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
}

pub fn set_meta_int(c: &Connection, key: &str, v: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, v],
    )?;
    Ok(())
}

pub fn set_meta_blob(c: &Connection, key: &str, v: &[u8]) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, v],
    )?;
    Ok(())
}

pub fn delete_meta(c: &Connection, key: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
    Ok(())
}

pub fn current_seq(c: &Connection) -> rusqlite::Result<u64> {
    Ok(u(meta_int(c, "seq")?.unwrap_or(0)))
}

pub fn retention_days(c: &Connection) -> rusqlite::Result<u32> {
    Ok(meta_int(c, "retention_days")?
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS))
}

pub fn vault_state(c: &Connection) -> rusqlite::Result<pb::VaultState> {
    let mut st = c.prepare_cached(
        "SELECT key, value FROM meta WHERE key IN ('seq','vault_key_version','migration','retention_days','purged_seq')",
    )?;
    let mut s = pb::VaultState {
        retention_days: DEFAULT_RETENTION_DAYS,
        ..Default::default()
    };
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let k: String = r.get(0)?;
        match k.as_str() {
            "seq" => s.seq = u(r.get(1)?),
            "vault_key_version" => s.key_version = u(r.get(1)?),
            "migration" => s.migration = true,
            "retention_days" => {
                s.retention_days =
                    u32::try_from(r.get::<_, i64>(1)?).unwrap_or(DEFAULT_RETENTION_DAYS)
            }
            "purged_seq" => s.purged_seq = u(r.get(1)?),
            _ => {}
        }
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: Vec<u8>,
    pub rev: u64,
    pub seq: u64,
    pub hash: Option<Vec<u8>>,
    pub size: u64,
    pub mtime: i64,
    pub deleted: bool,
    pub deleted_at: Option<i64>,
    pub folder: bool,
    pub renamed_from: Option<Vec<u8>>,
    pub renamed_to: Option<Vec<u8>>,
    pub updated_by: u32,
}

const FILE_COLS: &str = "path, rev, seq, hash, size, mtime, deleted, deleted_at, folder, renamed_from, renamed_to, updated_by";

fn file_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        path: r.get(0)?,
        rev: u(r.get(1)?),
        seq: u(r.get(2)?),
        hash: r.get(3)?,
        size: u(r.get(4)?),
        mtime: r.get(5)?,
        deleted: r.get::<_, i64>(6)? != 0,
        deleted_at: r.get(7)?,
        folder: r.get::<_, i64>(8)? != 0,
        renamed_from: r.get(9)?,
        renamed_to: r.get(10)?,
        updated_by: u32::try_from(r.get::<_, i64>(11)?).unwrap_or(0),
    })
}

fn decode_path(b: &[u8]) -> pb::Path {
    // Пути в БД пишет только этот модуль после валидации.
    canonical_decode(b).unwrap_or_default()
}

impl FileRow {
    pub fn is_live(&self) -> bool {
        !self.deleted
    }

    pub fn to_entry(&self) -> pb::Entry {
        pb::Entry {
            seq: self.seq,
            path: Some(decode_path(&self.path)),
            rev: self.rev,
            hash: self.hash.clone().unwrap_or_default(),
            size: self.size,
            mtime: self.mtime,
            deleted: self.deleted,
            folder: self.folder,
            renamed_from: self.renamed_from.as_deref().map(decode_path),
            updated_by: self.updated_by,
            deleted_at: self.deleted_at.unwrap_or(0),
            renamed_to: self.renamed_to.as_deref().map(decode_path),
        }
    }

    /// Пустая запись «пути нет» — для Conflict на отсутствующий путь.
    pub fn absent(path: Vec<u8>) -> FileRow {
        FileRow {
            path,
            rev: 0,
            seq: 0,
            hash: None,
            size: 0,
            mtime: 0,
            deleted: true,
            deleted_at: None,
            folder: false,
            renamed_from: None,
            renamed_to: None,
            updated_by: 0,
        }
    }
}

pub fn get_file(c: &Connection, path: &[u8]) -> rusqlite::Result<Option<FileRow>> {
    c.prepare_cached(&format!("SELECT {FILE_COLS} FROM files WHERE path = ?1"))?
        .query_row(params![path], file_row)
        .optional()
}

fn upsert_file(tx: &Transaction<'_>, f: &FileRow) -> rusqlite::Result<()> {
    tx.prepare_cached(&format!(
        "INSERT INTO files ({FILE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(path) DO UPDATE SET rev = excluded.rev, seq = excluded.seq, hash = excluded.hash,
           size = excluded.size, mtime = excluded.mtime, deleted = excluded.deleted,
           deleted_at = excluded.deleted_at, folder = excluded.folder,
           renamed_from = excluded.renamed_from, renamed_to = excluded.renamed_to,
           updated_by = excluded.updated_by"
    ))?
    .execute(params![
        f.path,
        i(f.rev),
        i(f.seq),
        f.hash,
        i(f.size),
        f.mtime,
        i64::from(f.deleted),
        f.deleted_at,
        i64::from(f.folder),
        f.renamed_from,
        f.renamed_to,
        i64::from(f.updated_by),
    ])?;
    Ok(())
}

fn insert_revision(tx: &Transaction<'_>, f: &FileRow, now: i64) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT OR REPLACE INTO revisions
         (path, rev, hash, size, mtime, seq, updated_by, deleted, folder, renamed_from, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        f.path,
        i(f.rev),
        f.hash,
        i(f.size),
        f.mtime,
        i(f.seq),
        i64::from(f.updated_by),
        i64::from(f.deleted),
        i64::from(f.folder),
        f.renamed_from,
        now,
    ])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Операции
// ---------------------------------------------------------------------------

pub struct OpsCtx<'a> {
    pub device: u32,
    pub now: i64,
    pub max_blob_size: u64,
    /// Есть ли блоб в хранилище (проверка ФС внутри транзакции: gc удаляет блобы
    /// только под той же блокировкой записи, так что ответ не устареет до коммита).
    pub blob_exists: &'a dyn Fn(&Hash) -> bool,
}

pub struct OpsOutcome {
    pub results: Vec<pb::OpResult>,
    pub state: pb::VaultState,
    pub changed: bool,
}

fn rejected(code: &str, message: impl Into<String>) -> pb::OpResult {
    pb::OpResult {
        result: Some(pb::op_result::Result::Rejected(pb::Rejected {
            code: code.to_owned(),
            message: message.into(),
        })),
    }
}

fn reject_path(e: &PathError) -> pb::OpResult {
    rejected(e.code(), e.to_string())
}

fn applied(rev: u64, seq: u64, noop: bool) -> pb::OpResult {
    pb::OpResult {
        result: Some(pb::op_result::Result::Applied(pb::Applied {
            rev,
            seq,
            noop,
        })),
    }
}

fn conflict(row: &FileRow, at_destination: bool) -> pb::OpResult {
    pb::OpResult {
        result: Some(pb::op_result::Result::Conflict(pb::Conflict {
            server: Some(row.to_entry()),
            at_destination,
        })),
    }
}

fn missing_blob() -> pb::OpResult {
    pb::OpResult {
        result: Some(pb::op_result::Result::MissingBlob(pb::MissingBlob {})),
    }
}

struct KeyState {
    has_key: bool,
    migration: bool,
}

impl KeyState {
    /// Открытые записи запрещены в зашифрованном vault'е вне миграции.
    fn forbids_plaintext(&self) -> bool {
        self.has_key && !self.migration
    }
}

/// Проверка пути операции: возвращает каноническое кодирование или отказ.
// Err — готовый результат операции; батч обрабатывается по одной операции, размер не важен.
#[allow(clippy::result_large_err)]
fn check_path(p: Option<&pb::Path>, ks: &KeyState) -> Result<Vec<u8>, pb::OpResult> {
    let Some(p) = p else {
        return Err(rejected("path_missing", "нет пути"));
    };
    validate_proto_path(p).map_err(|e| reject_path(&e))?;
    if !p.encrypted && ks.forbids_plaintext() {
        return Err(rejected(
            "plaintext_in_encrypted_vault",
            "vault зашифрован: открытые записи запрещены",
        ));
    }
    Ok(canonical_encode(p))
}

/// Применяет батч операций одной транзакцией `BEGIN IMMEDIATE`.
///
/// Порядок проверок для каждой операции (раздел 6.3):
/// 1. валидация пути и лимитов → `Rejected`;
/// 2. идемпотентность **раньше** `base_rev`: состояние уже совпадает → `Applied{noop}`,
///    `seq` не двигается;
/// 3. `Put`: блоба нет → `MissingBlob`;
/// 4. `base_rev` не совпал → `Conflict` с текущей записью;
/// 5. применить: `rev + 1`, новый `seq`, строка в `revisions`.
pub fn apply_ops(
    c: &mut Connection,
    ctx: &OpsCtx<'_>,
    ops: &[pb::Op],
) -> rusqlite::Result<OpsOutcome> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let start_seq = current_seq(&tx)?;
    let mut seq = start_seq;
    let ks = KeyState {
        has_key: meta_blob(&tx, "vault_key")?.is_some(),
        migration: meta_blob(&tx, "migration")?.is_some(),
    };
    let mut results = Vec::with_capacity(ops.len());
    for op in ops {
        let r = match &op.kind {
            None => rejected("op_empty", "пустая операция"),
            Some(pb::op::Kind::Put(p)) => op_put(&tx, ctx, &ks, &mut seq, p)?,
            Some(pb::op::Kind::Delete(d)) => op_delete(&tx, ctx, &mut seq, d)?,
            Some(pb::op::Kind::Rename(r)) => op_rename(&tx, ctx, &ks, &mut seq, r)?,
            Some(pb::op::Kind::Mkdir(m)) => op_mkdir(&tx, ctx, &ks, &mut seq, m)?,
        };
        results.push(r);
    }
    let changed = seq != start_seq;
    if changed {
        set_meta_int(&tx, "seq", i(seq))?;
    }
    let state = vault_state(&tx)?;
    tx.commit()?;
    Ok(OpsOutcome {
        results,
        state,
        changed,
    })
}

fn op_put(
    tx: &Transaction<'_>,
    ctx: &OpsCtx<'_>,
    ks: &KeyState,
    seq: &mut u64,
    p: &pb::Put,
) -> rusqlite::Result<pb::OpResult> {
    let key = match check_path(p.path.as_ref(), ks) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let Some(hash) = Hash::from_slice(&p.hash) else {
        return Ok(rejected("bad_hash", "хэш должен быть 32 байта"));
    };
    if p.size > ctx.max_blob_size {
        return Ok(rejected("too_large", "файл больше лимита сервера"));
    }
    let row = get_file(tx, &key)?;
    // 2. Идемпотентность раньше base_rev.
    if let Some(r) = &row
        && r.is_live()
        && !r.folder
        && r.hash.as_deref() == Some(&p.hash[..])
    {
        return Ok(applied(r.rev, r.seq, true));
    }
    // 3. Блоб должен быть загружен.
    if !(ctx.blob_exists)(&hash) {
        return Ok(missing_blob());
    }
    // 4. base_rev.
    let cur_rev = row.as_ref().map_or(0, |r| r.rev);
    let tomb = row.as_ref().is_none_or(|r| r.deleted);
    let ok = p.base_rev == cur_rev || (p.base_rev == 0 && tomb);
    let over_folder = row.as_ref().is_some_and(|r| r.is_live() && r.folder);
    if !ok || over_folder {
        let r = row.unwrap_or_else(|| FileRow::absent(key));
        return Ok(conflict(&r, false));
    }
    // 5. Применить.
    *seq += 1;
    let f = FileRow {
        path: key,
        rev: cur_rev + 1,
        seq: *seq,
        hash: Some(p.hash.clone()),
        size: p.size,
        mtime: p.mtime,
        deleted: false,
        deleted_at: None,
        folder: false,
        renamed_from: None,
        renamed_to: None,
        updated_by: ctx.device,
    };
    upsert_file(tx, &f)?;
    insert_revision(tx, &f, ctx.now)?;
    Ok(applied(f.rev, f.seq, false))
}

fn op_delete(
    tx: &Transaction<'_>,
    ctx: &OpsCtx<'_>,
    seq: &mut u64,
    d: &pb::Delete,
) -> rusqlite::Result<pb::OpResult> {
    // Удаление открытой записи в зашифрованном vault'е разрешено: оно только убирает
    // открытый текст.
    let ks = KeyState {
        has_key: false,
        migration: false,
    };
    let key = match check_path(d.path.as_ref(), &ks) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let row = get_file(tx, &key)?;
    let Some(row) = row.filter(FileRow::is_live) else {
        // Уже tombstone или пути не было: повтор удаления — no-op.
        let r = get_file(tx, &key)?;
        return Ok(applied(
            r.as_ref().map_or(0, |r| r.rev),
            r.as_ref().map_or(0, |r| r.seq),
            true,
        ));
    };
    if d.base_rev != row.rev {
        return Ok(conflict(&row, false));
    }
    *seq += 1;
    let f = FileRow {
        rev: row.rev + 1,
        seq: *seq,
        hash: None,
        size: 0,
        deleted: true,
        deleted_at: Some(ctx.now),
        renamed_from: None,
        renamed_to: None,
        updated_by: ctx.device,
        ..row
    };
    upsert_file(tx, &f)?;
    insert_revision(tx, &f, ctx.now)?;
    Ok(applied(f.rev, f.seq, false))
}

fn op_rename(
    tx: &Transaction<'_>,
    ctx: &OpsCtx<'_>,
    ks: &KeyState,
    seq: &mut u64,
    r: &pb::Rename,
) -> rusqlite::Result<pb::OpResult> {
    let from = match check_path(r.from.as_ref(), ks) {
        Ok(k) => k,
        Err(res) => return Ok(res),
    };
    let to = match check_path(r.to.as_ref(), ks) {
        Ok(k) => k,
        Err(res) => return Ok(res),
    };
    if from[0] != to[0] {
        return Ok(rejected(
            "rename_mode_mismatch",
            "переименование между открытым и зашифрованным путём",
        ));
    }
    if from == to {
        return Ok(rejected(
            "rename_same_path",
            "источник совпадает с назначением",
        ));
    }
    let src = get_file(tx, &from)?;
    let dst = get_file(tx, &to)?;
    // 2. Идемпотентность: уже переименовано.
    if let Some(d) = &dst
        && d.is_live()
        && d.renamed_from.as_deref() == Some(&from[..])
        && src.as_ref().is_none_or(|s| s.deleted)
    {
        return Ok(applied(d.rev, d.seq, true));
    }
    let Some(src) = src.filter(FileRow::is_live) else {
        let s = get_file(tx, &from)?.unwrap_or_else(|| FileRow::absent(from));
        return Ok(conflict(&s, false));
    };
    if r.base_rev != src.rev {
        return Ok(conflict(&src, false));
    }
    if let Some(d) = &dst
        && d.is_live()
    {
        // Занятое место назначения — конфликт, а не перезапись.
        return Ok(conflict(d, true));
    }
    // Назначение получает меньший seq: клиент, читающий лог по порядку, сначала
    // переименует файл локально, а tombstone источника потом окажется пустым.
    *seq += 1;
    let dst_rev = dst.as_ref().map_or(0, |d| d.rev) + 1;
    let moved = FileRow {
        path: to.clone(),
        rev: dst_rev,
        seq: *seq,
        hash: src.hash.clone(),
        size: src.size,
        mtime: src.mtime,
        deleted: false,
        deleted_at: None,
        folder: src.folder,
        renamed_from: Some(from.clone()),
        renamed_to: None,
        updated_by: ctx.device,
    };
    upsert_file(tx, &moved)?;
    insert_revision(tx, &moved, ctx.now)?;
    *seq += 1;
    let tomb = FileRow {
        rev: src.rev + 1,
        seq: *seq,
        hash: None,
        size: 0,
        deleted: true,
        deleted_at: Some(ctx.now),
        renamed_from: None,
        renamed_to: Some(to),
        updated_by: ctx.device,
        ..src
    };
    upsert_file(tx, &tomb)?;
    insert_revision(tx, &tomb, ctx.now)?;
    Ok(applied(moved.rev, moved.seq, false))
}

fn op_mkdir(
    tx: &Transaction<'_>,
    ctx: &OpsCtx<'_>,
    ks: &KeyState,
    seq: &mut u64,
    m: &pb::Mkdir,
) -> rusqlite::Result<pb::OpResult> {
    let key = match check_path(m.path.as_ref(), ks) {
        Ok(k) => k,
        Err(r) => return Ok(r),
    };
    let row = get_file(tx, &key)?;
    if let Some(r) = &row
        && r.is_live()
    {
        return Ok(if r.folder {
            applied(r.rev, r.seq, true)
        } else {
            conflict(r, false)
        });
    }
    *seq += 1;
    let f = FileRow {
        path: key,
        rev: row.as_ref().map_or(0, |r| r.rev) + 1,
        seq: *seq,
        hash: None,
        size: 0,
        mtime: ctx.now,
        deleted: false,
        deleted_at: None,
        folder: true,
        renamed_from: None,
        renamed_to: None,
        updated_by: ctx.device,
    };
    upsert_file(tx, &f)?;
    insert_revision(tx, &f, ctx.now)?;
    Ok(applied(f.rev, f.seq, false))
}

// ---------------------------------------------------------------------------
// Чтение
// ---------------------------------------------------------------------------

/// Дельта после `since`: (записи, has_more).
pub fn changes(
    c: &Connection,
    since: u64,
    limit: usize,
) -> rusqlite::Result<(Vec<pb::Entry>, bool)> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {FILE_COLS} FROM files WHERE seq > ?1 ORDER BY seq LIMIT ?2"
    ))?;
    let lim = i64::try_from(limit + 1).unwrap_or(i64::MAX);
    let mut out = Vec::with_capacity(limit.min(1024));
    for row in st.query_map(params![i(since), lim], file_row)? {
        out.push(row?.to_entry());
    }
    let has_more = out.len() > limit;
    out.truncate(limit);
    Ok((out, has_more))
}

fn revision(r: &rusqlite::Row<'_>) -> rusqlite::Result<pb::Revision> {
    let rf: Option<Vec<u8>> = r.get(8)?;
    let hash: Option<Vec<u8>> = r.get(1)?;
    Ok(pb::Revision {
        rev: u(r.get(0)?),
        hash: hash.unwrap_or_default(),
        size: u(r.get(2)?),
        mtime: r.get(3)?,
        seq: u(r.get(4)?),
        updated_by: u32::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
        deleted: r.get::<_, i64>(6)? != 0,
        folder: r.get::<_, i64>(7)? != 0,
        renamed_from: rf.as_deref().map(decode_path),
    })
}

/// История файла, от новых ревизий к старым.
pub fn history(c: &Connection, path: &[u8]) -> rusqlite::Result<Vec<pb::Revision>> {
    let mut st = c.prepare_cached(
        "SELECT rev, hash, size, mtime, seq, updated_by, deleted, folder, renamed_from
         FROM revisions WHERE path = ?1 ORDER BY rev DESC",
    )?;
    let rows = st.query_map(params![path], revision)?;
    rows.collect()
}

/// Удалённые файлы в окне хранения (без источников переименований и папок).
pub fn deleted(c: &Connection, now: i64) -> rusqlite::Result<Vec<pb::DeletedItem>> {
    let retention = i64::from(retention_days(c)?) * DAY_MS;
    let mut st = c.prepare_cached(&format!(
        "SELECT {FILE_COLS} FROM files WHERE deleted = 1 AND folder = 0 AND renamed_to IS NULL
         AND deleted_at IS NOT NULL ORDER BY deleted_at DESC"
    ))?;
    let rows: Vec<FileRow> = st.query_map([], file_row)?.collect::<Result<_, _>>()?;
    let mut last = c.prepare_cached(
        "SELECT rev, hash, size, mtime, seq, updated_by, deleted, folder, renamed_from
         FROM revisions WHERE path = ?1 AND deleted = 0 AND hash IS NOT NULL ORDER BY rev DESC LIMIT 1",
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let last_live = last.query_row(params![row.path], revision).optional()?;
        let Some(last_live) = last_live else { continue };
        let expires_at = row.deleted_at.unwrap_or(now) + retention;
        out.push(pb::DeletedItem {
            entry: Some(row.to_entry()),
            last_live: Some(last_live),
            expires_at,
        });
    }
    Ok(out)
}

/// Хэши блобов, на которые ссылаются строки пути (для последующей уборки).
fn path_hashes(tx: &Connection, path: &[u8]) -> rusqlite::Result<Vec<Vec<u8>>> {
    let mut st = tx.prepare_cached(
        "SELECT DISTINCT hash FROM revisions WHERE path = ?1 AND hash IS NOT NULL",
    )?;
    let rows = st.query_map(params![path], |r| r.get(0))?;
    rows.collect()
}

/// Окончательно стирает выбранные tombstone'ы вместе с историей. Возвращает число
/// стёртых путей и хэши, которые могли стать бесхозными.
pub fn purge_deleted(
    c: &mut Connection,
    paths: &[Vec<u8>],
) -> rusqlite::Result<(u64, Vec<Vec<u8>>)> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut n = 0;
    let mut hashes = Vec::new();
    let mut max_seq = 0u64;
    for p in paths {
        if let Some(row) = get_file(&tx, p)?
            && row.deleted
        {
            hashes.extend(path_hashes(&tx, p)?);
            tx.execute("DELETE FROM revisions WHERE path = ?1", params![p])?;
            tx.execute("DELETE FROM files WHERE path = ?1", params![p])?;
            max_seq = max_seq.max(row.seq);
            n += 1;
        }
    }
    bump_purged_seq(&tx, max_seq)?;
    tx.commit()?;
    Ok((n, hashes))
}

fn bump_purged_seq(tx: &Connection, s: u64) -> rusqlite::Result<()> {
    if s > 0 {
        let cur = u(meta_int(tx, "purged_seq")?.unwrap_or(0));
        if s > cur {
            set_meta_int(tx, "purged_seq", i(s))?;
        }
    }
    Ok(())
}

/// Sweeper: стирает tombstone'ы старше окна хранения. Источники переименований
/// (их история — начало истории переехавшего файла) оставляются `gc`.
pub fn sweep(c: &mut Connection, now: i64) -> rusqlite::Result<(u64, Vec<Vec<u8>>)> {
    let retention = i64::from(retention_days(c)?) * DAY_MS;
    let cutoff = now - retention;
    let paths: Vec<Vec<u8>> = {
        let mut st = c.prepare(
            "SELECT path FROM files WHERE deleted = 1 AND renamed_to IS NULL AND deleted_at < ?1",
        )?;
        st.query_map(params![cutoff], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    let res = purge_deleted(c, &paths)?;
    set_meta_int(c, "last_sweep", now)?;
    Ok(res)
}

/// Ссылается ли кто-нибудь на блоб.
pub fn blob_referenced(c: &Connection, hash: &[u8]) -> rusqlite::Result<bool> {
    let n: i64 = c.query_row(
        "SELECT (SELECT COUNT(*) FROM files WHERE hash = ?1) + (SELECT COUNT(*) FROM revisions WHERE hash = ?1)",
        params![hash],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn stats(c: &Connection) -> rusqlite::Result<pb::Stats> {
    let (files, folders, deleted, live_bytes): (i64, i64, i64, i64) = c.query_row(
        "SELECT
           COALESCE(SUM(CASE WHEN deleted = 0 AND folder = 0 THEN 1 ELSE 0 END), 0),
           COALESCE(SUM(CASE WHEN deleted = 0 AND folder = 1 THEN 1 ELSE 0 END), 0),
           COALESCE(SUM(CASE WHEN deleted = 1 THEN 1 ELSE 0 END), 0),
           COALESCE(SUM(CASE WHEN deleted = 0 THEN size ELSE 0 END), 0)
         FROM files",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let revisions: i64 = c.query_row("SELECT COUNT(*) FROM revisions", [], |r| r.get(0))?;
    Ok(pb::Stats {
        seq: current_seq(c)?,
        files: u(files),
        folders: u(folders),
        deleted: u(deleted),
        live_bytes: u(live_bytes),
        revisions: u(revisions),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Ключ шифрования и миграция
// ---------------------------------------------------------------------------

pub fn get_vault_key(c: &Connection) -> rusqlite::Result<pb::VaultKeyResponse> {
    let record = meta_blob(c, "vault_key")?.unwrap_or_default();
    let version = u(meta_int(c, "vault_key_version")?.unwrap_or(0));
    let migration =
        meta_blob(c, "migration")?.and_then(|b| pb::MigrationMarker::decode(&b[..]).ok());
    Ok(pb::VaultKeyResponse {
        record,
        version: if version == 0 && meta_blob(c, "vault_key")?.is_none() {
            0
        } else {
            version
        },
        migration,
    })
}

/// Записывает ключ, если версия совпала (`expected = 0` — записи быть не должно).
/// Возвращает новую версию или `None` при несовпадении.
pub fn put_vault_key(
    c: &mut Connection,
    record: &[u8],
    expected: u64,
) -> rusqlite::Result<Option<u64>> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists = meta_blob(&tx, "vault_key")?.is_some();
    let version = u(meta_int(&tx, "vault_key_version")?.unwrap_or(0));
    let current = if exists { version } else { 0 };
    if current != expected {
        return Ok(None);
    }
    let next = version + 1;
    set_meta_blob(&tx, "vault_key", record)?;
    set_meta_int(&tx, "vault_key_version", i(next))?;
    tx.commit()?;
    Ok(Some(next))
}

pub fn set_migration(c: &Connection, marker: &pb::MigrationMarker) -> rusqlite::Result<()> {
    set_meta_blob(c, "migration", &marker.encode_to_vec())
}

pub fn clear_migration(c: &Connection) -> rusqlite::Result<()> {
    delete_meta(c, "migration")
}

#[derive(Debug, PartialEq, Eq)]
pub enum PurgeOutcome {
    Done {
        purged: u64,
        hashes: Vec<Vec<u8>>,
    },
    NoMigration,
    /// Открытая запись изменилась после снимка миграции.
    PlaintextChanged,
}

/// Стирает открытые записи и их историю после перезаливки.
pub fn purge_plaintext(c: &mut Connection, max_seq: u64) -> rusqlite::Result<PurgeOutcome> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if meta_blob(&tx, "migration")?.is_none() {
        return Ok(PurgeOutcome::NoMigration);
    }
    // Открытые пути начинаются с байта режима 0x00.
    let newer: i64 = tx.query_row(
        "SELECT COUNT(*) FROM files WHERE substr(path, 1, 1) = x'00' AND seq > ?1 AND deleted = 0",
        params![i(max_seq)],
        |r| r.get(0),
    )?;
    if newer > 0 {
        return Ok(PurgeOutcome::PlaintextChanged);
    }
    let hashes: Vec<Vec<u8>> = {
        let mut st = tx.prepare(
            "SELECT DISTINCT hash FROM revisions WHERE substr(path, 1, 1) = x'00' AND hash IS NOT NULL",
        )?;
        st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    let max_purged: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM files WHERE substr(path, 1, 1) = x'00'",
        [],
        |r| r.get(0),
    )?;
    let purged = tx.execute("DELETE FROM files WHERE substr(path, 1, 1) = x'00'", [])?;
    tx.execute("DELETE FROM revisions WHERE substr(path, 1, 1) = x'00'", [])?;
    bump_purged_seq(&tx, u(max_purged))?;
    // Маркер снимается в той же транзакции: иначе между purge и снятием маркера
    // сервер ещё принимал бы открытые записи, и они остались бы навсегда.
    clear_migration(&tx)?;
    tx.commit()?;
    Ok(PurgeOutcome::Done {
        purged: purged as u64,
        hashes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfsync_core::path::VaultPath;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate_vault(&mut c).unwrap();
        c
    }

    fn p(s: &str) -> pb::Path {
        VaultPath::parse(s).unwrap().to_proto()
    }

    fn put(path: &str, base: u64, h: u8) -> pb::Op {
        pb::Op {
            kind: Some(pb::op::Kind::Put(pb::Put {
                path: Some(p(path)),
                base_rev: base,
                hash: vec![h; 32],
                size: 10,
                mtime: 1,
            })),
        }
    }

    fn del(path: &str, base: u64) -> pb::Op {
        pb::Op {
            kind: Some(pb::op::Kind::Delete(pb::Delete {
                path: Some(p(path)),
                base_rev: base,
            })),
        }
    }

    fn ren(from: &str, to: &str, base: u64) -> pb::Op {
        pb::Op {
            kind: Some(pb::op::Kind::Rename(pb::Rename {
                from: Some(p(from)),
                to: Some(p(to)),
                base_rev: base,
            })),
        }
    }

    fn run(c: &mut Connection, ops: Vec<pb::Op>) -> Vec<pb::op_result::Result> {
        let all = |_: &Hash| true;
        let ctx = OpsCtx {
            device: 1,
            now: 1000,
            max_blob_size: 1 << 20,
            blob_exists: &all,
        };
        apply_ops(c, &ctx, &ops)
            .unwrap()
            .results
            .into_iter()
            .map(|r| r.result.unwrap())
            .collect()
    }

    use pb::op_result::Result as R;

    #[test]
    fn put_then_idempotent_retry_does_not_move_seq() {
        let mut c = db();
        let r = run(&mut c, vec![put("a.md", 0, 1)]);
        assert_eq!(
            r[0],
            R::Applied(pb::Applied {
                rev: 1,
                seq: 1,
                noop: false
            })
        );
        // повтор с устаревшим base_rev — no-op, а не конфликт
        let r = run(&mut c, vec![put("a.md", 0, 1)]);
        assert_eq!(
            r[0],
            R::Applied(pb::Applied {
                rev: 1,
                seq: 1,
                noop: true
            })
        );
        assert_eq!(current_seq(&c).unwrap(), 1);
    }

    #[test]
    fn stale_base_rev_conflicts() {
        let mut c = db();
        run(&mut c, vec![put("a.md", 0, 1), put("a.md", 1, 2)]);
        let r = run(&mut c, vec![put("a.md", 1, 3)]);
        match &r[0] {
            R::Conflict(cf) => {
                let e = cf.server.as_ref().unwrap();
                assert_eq!(e.rev, 2);
                assert_eq!(e.hash, vec![2; 32]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_blob_checked_before_base_rev() {
        let mut c = db();
        let none = |_: &Hash| false;
        let ctx = OpsCtx {
            device: 1,
            now: 0,
            max_blob_size: 100,
            blob_exists: &none,
        };
        let out = apply_ops(&mut c, &ctx, &[put("a.md", 7, 1)]).unwrap();
        assert!(matches!(out.results[0].result, Some(R::MissingBlob(_))));
        assert!(!out.changed);
    }

    #[test]
    fn delete_tombstone_and_restore() {
        let mut c = db();
        run(&mut c, vec![put("a.md", 0, 1)]);
        let r = run(&mut c, vec![del("a.md", 1)]);
        assert_eq!(
            r[0],
            R::Applied(pb::Applied {
                rev: 2,
                seq: 2,
                noop: false
            })
        );
        let r = run(&mut c, vec![del("a.md", 1)]);
        assert_eq!(
            r[0],
            R::Applied(pb::Applied {
                rev: 2,
                seq: 2,
                noop: true
            })
        );
        let d = deleted(&c, 2000).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].last_live.as_ref().unwrap().hash, vec![1; 32]);
        // восстановление: Put старого хэша с base_rev tombstone'а
        let r = run(&mut c, vec![put("a.md", 2, 1)]);
        assert_eq!(
            r[0],
            R::Applied(pb::Applied {
                rev: 3,
                seq: 3,
                noop: false
            })
        );
        assert!(deleted(&c, 2000).unwrap().is_empty());
        // base_rev = 0 поверх tombstone тоже допустим
        run(&mut c, vec![del("a.md", 3)]);
        let r = run(&mut c, vec![put("a.md", 0, 5)]);
        assert!(matches!(r[0], R::Applied(_)));
        assert_eq!(history(&c, &canonical_encode(&p("a.md"))).unwrap().len(), 5);
    }

    #[test]
    fn delete_nonexistent_is_noop() {
        let mut c = db();
        let r = run(&mut c, vec![del("ghost.md", 0)]);
        assert_eq!(
            r[0],
            R::Applied(pb::Applied {
                rev: 0,
                seq: 0,
                noop: true
            })
        );
    }

    #[test]
    fn rename_semantics() {
        let mut c = db();
        run(&mut c, vec![put("a.md", 0, 1), put("b.md", 0, 2)]);
        // занятое назначение
        match &run(&mut c, vec![ren("a.md", "b.md", 1)])[0] {
            R::Conflict(cf) => assert!(cf.at_destination),
            o => panic!("{o:?}"),
        }
        // неверный base_rev источника
        assert!(matches!(
            run(&mut c, vec![ren("a.md", "c.md", 9)])[0],
            R::Conflict(_)
        ));
        // успешное
        let r = run(&mut c, vec![ren("a.md", "c.md", 1)]);
        let R::Applied(a) = &r[0] else { panic!() };
        assert!(!a.noop);
        let dst = get_file(&c, &canonical_encode(&p("c.md")))
            .unwrap()
            .unwrap();
        let src = get_file(&c, &canonical_encode(&p("a.md")))
            .unwrap()
            .unwrap();
        assert_eq!(dst.renamed_from, Some(canonical_encode(&p("a.md"))));
        assert!(src.deleted);
        assert!(dst.seq < src.seq, "назначение раньше tombstone'а источника");
        // повтор — no-op
        let r = run(&mut c, vec![ren("a.md", "c.md", 1)]);
        assert!(matches!(r[0], R::Applied(pb::Applied { noop: true, .. })));
        // источник переименования не считается удалённым
        assert!(deleted(&c, 2000).unwrap().is_empty());
        // назначение — tombstone: можно
        run(&mut c, vec![del("b.md", 1)]);
        assert!(matches!(
            run(&mut c, vec![ren("c.md", "b.md", 1)])[0],
            R::Applied(_)
        ));
    }

    #[test]
    fn case_only_rename() {
        let mut c = db();
        run(&mut c, vec![put("note.md", 0, 1)]);
        let r = run(&mut c, vec![ren("note.md", "Note.md", 1)]);
        assert!(matches!(r[0], R::Applied(pb::Applied { noop: false, .. })));
        let (entries, _) = changes(&c, 1, 100).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path.as_ref().unwrap().segments[0], b"Note.md");
        assert!(entries[1].deleted);
    }

    #[test]
    fn rejects_invalid_paths() {
        let mut c = db();
        let nfd = pb::Op {
            kind: Some(pb::op::Kind::Put(pb::Put {
                path: Some(pb::Path {
                    segments: vec!["e\u{301}.md".as_bytes().to_vec()],
                    encrypted: false,
                }),
                base_rev: 0,
                hash: vec![1; 32],
                size: 1,
                mtime: 0,
            })),
        };
        match &run(&mut c, vec![nfd])[0] {
            R::Rejected(r) => assert_eq!(r.code, "path_not_nfc"),
            o => panic!("{o:?}"),
        }
        let dots = pb::Op {
            kind: Some(pb::op::Kind::Mkdir(pb::Mkdir {
                path: Some(pb::Path {
                    segments: vec![b"..".to_vec()],
                    encrypted: false,
                }),
            })),
        };
        assert!(matches!(run(&mut c, vec![dots])[0], R::Rejected(_)));
        assert_eq!(current_seq(&c).unwrap(), 0);
    }

    #[test]
    fn folders() {
        let mut c = db();
        let mk = |s: &str| pb::Op {
            kind: Some(pb::op::Kind::Mkdir(pb::Mkdir { path: Some(p(s)) })),
        };
        assert!(matches!(
            run(&mut c, vec![mk("dir")])[0],
            R::Applied(pb::Applied { noop: false, .. })
        ));
        assert!(matches!(
            run(&mut c, vec![mk("dir")])[0],
            R::Applied(pb::Applied { noop: true, .. })
        ));
        // файл поверх папки — конфликт
        assert!(matches!(
            run(&mut c, vec![put("dir", 0, 1)])[0],
            R::Conflict(_)
        ));
        let (e, _) = changes(&c, 0, 10).unwrap();
        assert!(e[0].folder);
    }

    #[test]
    fn changes_pagination() {
        let mut c = db();
        let ops: Vec<_> = (0..10)
            .map(|n| put(&format!("f{n}.md"), 0, n as u8))
            .collect();
        run(&mut c, ops);
        let (e, more) = changes(&c, 0, 4).unwrap();
        assert_eq!(e.len(), 4);
        assert!(more);
        let (e2, more2) = changes(&c, e[3].seq, 100).unwrap();
        assert_eq!(e2.len(), 6);
        assert!(!more2);
    }

    #[test]
    fn plaintext_forbidden_after_key() {
        let mut c = db();
        assert_eq!(put_vault_key(&mut c, b"rec", 0).unwrap(), Some(1));
        assert_eq!(put_vault_key(&mut c, b"rec2", 0).unwrap(), None);
        match &run(&mut c, vec![put("a.md", 0, 1)])[0] {
            R::Rejected(r) => assert_eq!(r.code, "plaintext_in_encrypted_vault"),
            o => panic!("{o:?}"),
        }
        set_migration(&c, &pb::MigrationMarker::default()).unwrap();
        assert!(matches!(
            run(&mut c, vec![put("a.md", 0, 1)])[0],
            R::Applied(_)
        ));
        assert_eq!(put_vault_key(&mut c, b"rec2", 1).unwrap(), Some(2));
    }

    #[test]
    fn purge_plaintext_guard() {
        let mut c = db();
        run(&mut c, vec![put("a.md", 0, 1)]);
        assert_eq!(
            purge_plaintext(&mut c, 10).unwrap(),
            PurgeOutcome::NoMigration
        );
        set_migration(&c, &pb::MigrationMarker::default()).unwrap();
        let enc = pb::Op {
            kind: Some(pb::op::Kind::Put(pb::Put {
                path: Some(pb::Path {
                    segments: vec![vec![9; 32]],
                    encrypted: true,
                }),
                base_rev: 0,
                hash: vec![7; 32],
                size: 1,
                mtime: 0,
            })),
        };
        run(&mut c, vec![enc]);
        // открытый файл записан после снимка (seq 1 > max_seq 0)
        assert_eq!(
            purge_plaintext(&mut c, 0).unwrap(),
            PurgeOutcome::PlaintextChanged
        );
        match purge_plaintext(&mut c, 2).unwrap() {
            PurgeOutcome::Done { purged, hashes } => {
                assert_eq!(purged, 1);
                assert_eq!(hashes, vec![vec![1u8; 32]]);
            }
            o => panic!("{o:?}"),
        }
        let (e, _) = changes(&c, 0, 10).unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].path.as_ref().unwrap().encrypted);
        assert_eq!(vault_state(&c).unwrap().purged_seq, 1);
    }

    /// После purge маркера нет: открытая запись в окне «purge прошёл, маркер ещё
    /// стоит» осталась бы на сервере навсегда.
    #[test]
    fn purge_clears_marker_and_closes_plaintext() {
        let mut c = db();
        run(&mut c, vec![put("a.md", 0, 1)]);
        assert_eq!(put_vault_key(&mut c, b"rec", 0).unwrap(), Some(1));
        set_migration(&c, &pb::MigrationMarker::default()).unwrap();
        assert!(matches!(
            purge_plaintext(&mut c, 1).unwrap(),
            PurgeOutcome::Done { .. }
        ));
        assert!(meta_blob(&c, "migration").unwrap().is_none());
        match &run(&mut c, vec![put("b.md", 0, 2)])[0] {
            R::Rejected(r) => assert_eq!(r.code, "plaintext_in_encrypted_vault"),
            o => panic!("{o:?}"),
        }
        assert_eq!(
            purge_plaintext(&mut c, 1).unwrap(),
            PurgeOutcome::NoMigration
        );
    }

    #[test]
    fn sweep_removes_expired_tombstones() {
        let mut c = db();
        run(&mut c, vec![put("a.md", 0, 1), put("b.md", 0, 2)]);
        run(&mut c, vec![del("a.md", 1), ren("b.md", "c.md", 1)]);
        let far = 1000 + 31 * DAY_MS;
        let (n, hashes) = sweep(&mut c, far).unwrap();
        assert_eq!(n, 1);
        assert_eq!(hashes, vec![vec![1u8; 32]]);
        assert!(
            get_file(&c, &canonical_encode(&p("a.md")))
                .unwrap()
                .is_none()
        );
        // источник переименования не тронут
        assert!(
            get_file(&c, &canonical_encode(&p("b.md")))
                .unwrap()
                .is_some()
        );
        assert!(!blob_referenced(&c, &[1; 32]).unwrap());
        assert!(blob_referenced(&c, &[2; 32]).unwrap());
        assert_eq!(meta_int(&c, "last_sweep").unwrap(), Some(far));
    }
}
