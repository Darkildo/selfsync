//! Общее состояние движка и контекст задач: типизированные обёртки над действиями.
//!
//! Правило: заимствование `RefCell` никогда не держится через `.await`.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use notesync_proto::v1 as pb;

use super::runtime::Hub;
use super::types::*;
use crate::crypto::{MasterKey, VaultKeys};
use crate::exclude::Excludes;
use crate::hash::Hash;
use crate::index::{Index, VaultMode};
use crate::path::VaultPath;

/// Ошибка цикла синка.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    #[error("сеть: {0}")]
    Network(String),
    #[error("ошибка сервера {status}: {code}")]
    Server { status: u16, code: String },
    #[error("HTTP {status}: {code} {message}")]
    Http { status: u16, code: String, message: String },
    #[error("токен не принят")]
    Unauthorized,
    #[error("протокол не поддерживается сервером")]
    ProtoUnsupported(u32),
    #[error("ответ сервера не разбирается: {0}")]
    Protocol(String),
    #[error("ввод-вывод: {0}")]
    Io(String),
    #[error("синк приостановлен: {0}")]
    Paused(String),
    #[error("синк остановлен: {0}")]
    Blocked(String),
    #[error("блоб не найден на сервере")]
    BlobGone,
    #[error("данные повреждены: {0}")]
    Corrupt(String),
    /// Режим vault'а сменился посреди цикла — начать цикл заново.
    #[error("перезапуск цикла")]
    Restart,
}

pub type SyncResult<T> = Result<T, SyncError>;

/// Состояние движка.
pub(crate) struct State {
    pub cfg: EngineConfig,
    pub excludes: Excludes,
    pub index: Index,
    pub keys: Option<VaultKeys>,
    pub master: Option<MasterKey>,
    /// Ключ сверен с записью на сервере.
    pub keys_verified: bool,
    pub now: i64,
    pub status: SyncStatus,

    // Планирование.
    pub visible: bool,
    pub started: bool,
    /// Пути из событий, которые нужно перечитать.
    pub dirty: BTreeSet<String>,
    /// Переименования из событий, ещё не внесённые в индекс.
    pub renames: Vec<(String, String)>,
    pub need_full_scan: bool,
    pub last_full_scan: i64,
    pub sync_due: Option<i64>,
    pub sync_running: bool,
    pub poll_interval: u64,
    pub next_poll: Option<i64>,
    pub backoff_ms: u64,
    pub last_activity: i64,
    pub wake_at: Option<i64>,
    /// `/v1/wait` отвечает мгновенно (CGI) — переключиться на опрос.
    pub wait_disabled: bool,

    /// Синк остановлен до вмешательства пользователя (код причины).
    pub blocked: Option<String>,
    /// Синк приостановлен (ждём пароль или окончания чужой миграции).
    pub paused: Option<String>,
    pub server_state: Option<pb::VaultState>,

    // Запросы пользователя к задаче синка.
    pub enable: Option<(String, bool)>,

    /// Id устройства (из /v1/devices; для отличения своего эха не обязателен).
    pub device_id: u32,
    /// О каких путях уже предупредили (не повторять уведомление).
    pub notified: BTreeSet<String>,
    /// Запись, отложенная по временной причине: курсор не уходит дальше неё.
    pub hold_seq: Option<u64>,
}

impl State {
    pub fn new(cfg: EngineConfig, index: Index) -> State {
        let excludes = build_excludes(&cfg);
        let poll = cfg.poll_active_ms;
        State {
            cfg,
            excludes,
            index,
            keys: None,
            master: None,
            keys_verified: false,
            now: 0,
            status: SyncStatus::default(),
            visible: true,
            started: false,
            dirty: BTreeSet::new(),
            renames: Vec::new(),
            need_full_scan: true,
            last_full_scan: 0,
            sync_due: None,
            sync_running: false,
            poll_interval: poll,
            next_poll: None,
            backoff_ms: 0,
            last_activity: 0,
            wake_at: None,
            wait_disabled: false,
            blocked: None,
            paused: None,
            server_state: None,
            enable: None,
            device_id: 0,
            notified: BTreeSet::new(),
            hold_seq: None,
        }
    }

    pub fn encrypted(&self) -> bool {
        matches!(self.index.mode, VaultMode::Encrypted { .. })
    }

    pub fn is_excluded(&self, path: &str) -> bool {
        self.excludes.is_excluded(path) || path.ends_with(TEMP_SUFFIX)
    }

    /// Ключи для содержимого текущего режима.
    pub fn content_keys(&self) -> SyncResult<Option<&VaultKeys>> {
        if self.encrypted() {
            self.keys
                .as_ref()
                .map(Some)
                .ok_or_else(|| SyncError::Paused("need_password".into()))
        } else {
            Ok(None)
        }
    }

    /// Локальный путь → путь на сервере.
    pub fn to_server(&self, p: &VaultPath) -> SyncResult<pb::Path> {
        match self.content_keys()? {
            Some(k) => Ok(k.encrypt_path(p)),
            None => Ok(p.to_proto()),
        }
    }

    /// Путь с сервера → локальный. Записи «не того» режима (остатки миграции)
    /// пропускаются.
    pub fn from_server(&self, p: &pb::Path) -> Option<VaultPath> {
        if p.encrypted != self.encrypted() {
            return None;
        }
        if p.encrypted {
            self.keys.as_ref()?.decrypt_path(p).ok()
        } else {
            VaultPath::from_segments(&p.segments).ok()
        }
    }

}

pub(crate) fn build_excludes(cfg: &EngineConfig) -> Excludes {
    let mut e = Excludes::new(&cfg.excludes);
    for h in &cfg.hard_excludes {
        e.push(h);
    }
    e
}

/// Суффикс временных файлов атомарной записи (исключается всегда).
pub const TEMP_SUFFIX: &str = ".notesync-tmp";

/// Порог «маленького» блоба: одним запросом, целиком в памяти.
pub const SMALL_BLOB: u64 = 8 * 1024 * 1024;
/// Размер части resumable-загрузки и Range-скачивания.
pub const PART: u64 = 4 * 1024 * 1024;
/// Размер чтения локального файла частями.
pub const READ_CHUNK: u64 = 4 * 1024 * 1024;

pub const HTTP_TIMEOUT_MS: u64 = 60_000;
pub const TRANSFER_TIMEOUT_MS: u64 = 300_000;

/// Контекст задачи: доступ к состоянию и вводу-выводу.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub hub: Rc<Hub>,
    pub st: Rc<RefCell<State>>,
}

impl Ctx {
    pub fn with<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.st.borrow())
    }

    pub fn with_mut<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.st.borrow_mut())
    }

    pub fn now(&self) -> i64 {
        self.st.borrow().now
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.hub.emit(Action::Log {
            level,
            message: message.into(),
        });
    }

    pub fn notify(&self, notice: Notice) {
        self.hub.emit(Action::Notify { notice });
    }

    pub async fn http(&self, req: HttpRequest) -> SyncResult<(u16, Vec<(String, String)>, Vec<u8>)> {
        match self.hub.submit(|id| Action::Http { id, req }).await {
            IoResult::Http { status, headers, body } => Ok((status, headers, body)),
            IoResult::Failed { message } => Err(SyncError::Network(message)),
            other => Err(SyncError::Io(format!("неожиданный ответ на HTTP: {other:?}"))),
        }
    }

    pub async fn list(&self) -> SyncResult<Vec<FileMeta>> {
        match self.hub.submit(|id| Action::List { id }).await {
            IoResult::Listing { files } => Ok(files),
            other => Err(io_err("list", other)),
        }
    }

    pub async fn stat(&self, path: &str) -> SyncResult<Option<FileMeta>> {
        let path = path.to_owned();
        match self.hub.submit(|id| Action::Stat { id, path }).await {
            IoResult::Stat { meta } => Ok(meta),
            IoResult::NotFound => Ok(None),
            other => Err(io_err("stat", other)),
        }
    }

    /// Читает файл; `None` — файла нет.
    pub async fn read(&self, path: &str, offset: u64, len: Option<u64>) -> SyncResult<Option<Vec<u8>>> {
        let path = path.to_owned();
        match self.hub.submit(|id| Action::Read { id, path, offset, len }).await {
            IoResult::Data { data } => Ok(Some(data)),
            IoResult::NotFound => Ok(None),
            other => Err(io_err("read", other)),
        }
    }

    /// Атомарная запись. `Ok(None)` — условие не выполнено (файл изменился).
    pub async fn write(&self, path: &str, data: Vec<u8>, expect: Expect) -> SyncResult<Option<FileMeta>> {
        let p = path.to_owned();
        match self.hub.submit(|id| Action::Write { id, path: p, data, expect }).await {
            IoResult::Stat { meta } => Ok(meta.or_else(|| Some(FileMeta { path: path.to_owned(), size: 0, mtime: 0, dir: false }))),
            IoResult::Done => Ok(Some(FileMeta { path: path.to_owned(), size: 0, mtime: 0, dir: false })),
            IoResult::Precondition => Ok(None),
            other => Err(io_err("write", other)),
        }
    }

    pub async fn write_temp(&self, temp: &str, offset: u64, data: Vec<u8>) -> SyncResult<()> {
        let temp = temp.to_owned();
        match self.hub.submit(|id| Action::WriteTemp { id, temp, offset, data }).await {
            IoResult::Done => Ok(()),
            other => Err(io_err("write_temp", other)),
        }
    }

    pub async fn read_temp(&self, temp: &str, offset: u64, len: u64) -> SyncResult<Option<Vec<u8>>> {
        let temp = temp.to_owned();
        match self.hub.submit(|id| Action::ReadTemp { id, temp, offset, len }).await {
            IoResult::Data { data } => Ok(Some(data)),
            IoResult::NotFound => Ok(None),
            other => Err(io_err("read_temp", other)),
        }
    }

    pub async fn commit_temp(&self, temp: &str, path: &str, expect: Expect) -> SyncResult<Option<FileMeta>> {
        let (t, p) = (temp.to_owned(), path.to_owned());
        match self.hub.submit(|id| Action::CommitTemp { id, temp: t, path: p, expect }).await {
            IoResult::Stat { meta } => Ok(meta.or_else(|| Some(FileMeta { path: path.to_owned(), size: 0, mtime: 0, dir: false }))),
            IoResult::Done => Ok(Some(FileMeta { path: path.to_owned(), size: 0, mtime: 0, dir: false })),
            IoResult::Precondition => Ok(None),
            other => Err(io_err("commit_temp", other)),
        }
    }

    pub async fn delete_temp(&self, temp: &str) -> SyncResult<()> {
        let temp = temp.to_owned();
        match self.hub.submit(|id| Action::DeleteTemp { id, temp }).await {
            IoResult::Done | IoResult::NotFound => Ok(()),
            other => Err(io_err("delete_temp", other)),
        }
    }

    /// В корзину. `Ok(false)` — файл изменился (условие не выполнено).
    pub async fn trash(&self, path: &str, expect: Expect) -> SyncResult<bool> {
        let path = path.to_owned();
        match self.hub.submit(|id| Action::Trash { id, path, expect }).await {
            IoResult::Done | IoResult::NotFound => Ok(true),
            IoResult::Precondition => Ok(false),
            other => Err(io_err("trash", other)),
        }
    }

    /// Переименование. `Ok(None)` — назначение занято или источника нет.
    pub async fn rename(&self, from: &str, to: &str) -> SyncResult<Option<FileMeta>> {
        let (f, t) = (from.to_owned(), to.to_owned());
        match self.hub.submit(|id| Action::Rename { id, from: f, to: t }).await {
            IoResult::Stat { meta } => Ok(meta.or_else(|| Some(FileMeta { path: to.to_owned(), size: 0, mtime: 0, dir: false }))),
            IoResult::Done => Ok(Some(FileMeta { path: to.to_owned(), size: 0, mtime: 0, dir: false })),
            IoResult::Precondition | IoResult::NotFound => Ok(None),
            other => Err(io_err("rename", other)),
        }
    }

    pub async fn mkdir(&self, path: &str) -> SyncResult<()> {
        let path = path.to_owned();
        match self.hub.submit(|id| Action::Mkdir { id, path }).await {
            IoResult::Done | IoResult::Stat { .. } => Ok(()),
            other => Err(io_err("mkdir", other)),
        }
    }

    /// Удалить пустую папку. `Ok(false)` — не пуста.
    pub async fn rmdir(&self, path: &str) -> SyncResult<bool> {
        let path = path.to_owned();
        match self.hub.submit(|id| Action::Rmdir { id, path }).await {
            IoResult::Done | IoResult::NotFound => Ok(true),
            IoResult::Precondition => Ok(false),
            other => Err(io_err("rmdir", other)),
        }
    }

    /// Чекпоинт индекса.
    pub async fn save(&self) -> SyncResult<()> {
        let data = self.with(|s| s.index.encode());
        match self.hub.submit(|id| Action::SaveIndex { id, data }).await {
            IoResult::Done => Ok(()),
            other => Err(io_err("save_index", other)),
        }
    }

    pub async fn cache_read(&self, h: &Hash) -> Option<Vec<u8>> {
        let key = h.to_hex();
        match self.hub.submit(|id| Action::CacheRead { id, key }).await {
            IoResult::Data { data } if Hash::of(&data) == *h => Some(data),
            _ => None,
        }
    }

    /// Кладёт базовую версию текста в кэш с вытеснением старых.
    pub async fn cache_put(&self, h: Hash, data: Vec<u8>) {
        let size = data.len() as u64;
        let (exists, evict) = self.with_mut(|s| {
            if let Some(pos) = s.index.cache.iter().position(|(x, _)| *x == h) {
                let e = s.index.cache.remove(pos);
                s.index.cache.push(e);
                return (true, Vec::new());
            }
            s.index.cache.push((h, size));
            let limit = s.cfg.base_cache_bytes;
            let mut total: u64 = s.index.cache.iter().map(|x| x.1).sum();
            let mut evict = Vec::new();
            while total > limit && s.index.cache.len() > 1 {
                let (eh, es) = s.index.cache.remove(0);
                total -= es;
                evict.push(eh);
            }
            (false, evict)
        });
        if !exists {
            let key = h.to_hex();
            let _ = self.hub.submit(|id| Action::CacheWrite { id, key, data }).await;
        }
        for e in evict {
            let key = e.to_hex();
            let _ = self.hub.submit(|id| Action::CacheDelete { id, key }).await;
        }
    }

    pub fn set_status(&self, f: impl FnOnce(&mut SyncStatus)) {
        let status = self.with_mut(|s| {
            f(&mut s.status);
            s.status.pending = u32::try_from(s.index.pending_count()).unwrap_or(u32::MAX);
            s.status.conflicts = u32::try_from(s.index.conflicts.len()).unwrap_or(u32::MAX);
            s.status.encrypted = s.encrypted();
            s.status.clone()
        });
        self.hub.emit(Action::Status { status });
    }
}

fn io_err(op: &str, r: IoResult) -> SyncError {
    match r {
        IoResult::Failed { message } => SyncError::Io(format!("{op}: {message}")),
        other => SyncError::Io(format!("{op}: неожиданный ответ {other:?}")),
    }
}
