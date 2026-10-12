//! Общее состояние движка и контекст задач: типизированные обёртки над действиями.
//!
//! Правило: заимствование `RefCell` никогда не держится через `.await`.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use selfsync_proto::v1 as pb;
use selfsync_proto::{HEADER_CLIENT, HEADER_PROTO, PROTO_VERSION, parse_proto};

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
    Http {
        status: u16,
        code: String,
        message: String,
    },
    #[error("токен не принят")]
    Unauthorized,
    #[error("устройство подключено только для чтения")]
    ReadOnly,
    #[error("слишком много неудачных попыток, повтор через {retry_after_s} с")]
    RateLimited { retry_after_s: u64 },
    #[error("протокол не поддерживается сервером")]
    ProtoUnsupported(u32),
    /// Сервер старее клиента и не умеет нужного (текст — что именно и что делать).
    #[error("{0}")]
    ServerOutdated(String),
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
    /// `cfg.no_merge`, разобранный.
    pub no_merge: Excludes,
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
    /// Сервер ответил 429: до этого момента ни синка, ни long-poll (иначе клиент
    /// сам продлевает свою блокировку). «Синхронизировать сейчас» не отменяет.
    pub rate_limited_until: Option<i64>,
    /// Устройство только для чтения: из `VaultState` или по ответу 403 `read_only`
    /// (старый сервер поля не знает). Не сохраняется: узнаётся заново на первом
    /// запросе сессии, до любой отправки.
    pub read_only: bool,
    /// Узнали по 403 — `VaultState` старого сервера этого не отменит.
    pub read_only_forced: bool,
    /// Уведомление `ReadOnly` уже показано в этой сессии.
    pub read_only_notified: bool,
    /// Уведомление `StorageDamaged` показано; сбрасывается, когда повреждений нет.
    pub storage_notified: bool,
    /// Что изменил текущий цикл: локальные пути, тронутые синком, и отправленные.
    /// В конце цикла — `Notice::Synced`.
    pub pulled: BTreeSet<String>,
    pub pushed: BTreeSet<String>,

    /// Синк остановлен до вмешательства пользователя (код причины).
    pub blocked: Option<String>,
    /// Синк приостановлен (ждём пароль или окончания чужой миграции).
    pub paused: Option<String>,
    pub server_state: Option<pb::VaultState>,

    // Запросы пользователя к задаче синка.
    /// Включить шифрование: пароль, запомнить ключ, создать фразу восстановления.
    pub enable: Option<(String, bool, bool)>,
    /// Сменить ключ шифрования: пароль, запомнить новый ключ.
    pub rekey: Option<(String, bool)>,
    /// Ключи прежнего поколения, пока идёт смена ключа (из записи, под новым).
    pub prev_keys: Option<VaultKeys>,
    /// Поколение мастер-ключа в записи на сервере и есть ли в ней прежний ключ
    /// (смена ключа не завершена) — по последней проверке ключа.
    pub record_epoch: u32,
    pub record_prev: bool,
    /// Видели чужую смену ключа: после неё — перечитать запись.
    pub rekey_seen: bool,
    /// Версия записи ключа, по которой ключ проверен: другая на сервере —
    /// проверить заново (пароль сменили или ключ сменили).
    pub verified_version: u64,

    /// Id устройства (из /v1/devices; для отличения своего эха не обязателен).
    pub device_id: u32,
    /// О каких путях уже предупредили (не повторять уведомление).
    pub notified: BTreeSet<String>,
    /// Запись, отложенная по временной причине: курсор не уходит дальше неё.
    pub hold_seq: Option<u64>,
    /// Временные файлы забытых передач (путь стал исключённым) — удалить в цикле.
    pub stale_temps: Vec<String>,
    /// Правила исключений сменились (Configure): `excludes` перестраиваются из
    /// `cfg` и применяются к индексу в начале следующего цикла. Не сразу: идущий
    /// цикл читал бы журнал уже по новым правилам (пропустил бы записи, ставшие
    /// исключёнными, и увёл курсор за них) и в конце записал бы свой курсор поверх
    /// запрошенного повтора.
    pub rules_changed: bool,
    /// Пропущенные файлы, которые попросили скачать (в начале ближайшего цикла).
    pub download_queue: BTreeSet<String>,
    /// Запрос сервера на блобы разобран: сколько их было, время `verify` на сервере
    /// и когда разобран (см. `repair`).
    pub wanted_checked: Option<(u32, i64, i64)>,
}

impl State {
    pub fn new(cfg: EngineConfig, index: Index) -> State {
        let excludes = build_excludes(&cfg);
        let no_merge = Excludes::new(&cfg.no_merge);
        let poll = cfg.poll_active_ms;
        let mut s = State {
            cfg,
            excludes,
            no_merge,
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
            rate_limited_until: None,
            read_only: false,
            read_only_forced: false,
            read_only_notified: false,
            storage_notified: false,
            pulled: BTreeSet::new(),
            pushed: BTreeSet::new(),
            blocked: None,
            paused: None,
            server_state: None,
            enable: None,
            rekey: None,
            prev_keys: None,
            record_epoch: 1,
            record_prev: false,
            rekey_seen: false,
            verified_version: 0,
            device_id: 0,
            notified: BTreeSet::new(),
            hold_seq: None,
            stale_temps: Vec::new(),
            rules_changed: false,
            download_queue: BTreeSet::new(),
            wanted_checked: None,
        };
        // Правила могли смениться между запусками (CLI правят конфиг при
        // остановленной службе): событие Configure тогда не придёт.
        s.apply_rules();
        s
    }

    /// Применить текущие правила исключений к индексу: ставшее исключённым —
    /// забыть (без операций на сервер, локальные файлы не трогаются), если что-то
    /// стало включённым — перечитать журнал с начала, чтобы подтянуть пропущенное.
    /// Возвращает, запрошен ли повтор журнала.
    pub fn apply_rules(&mut self) -> bool {
        let lines = rule_lines(&self.cfg);
        let narrowed = self
            .index
            .excludes
            .as_ref()
            .is_some_and(|old| rules_narrowed(old, &lines));
        self.forget_excluded();
        // Снова включённое — уже не «можно удалить»: оно синхронизируется.
        let ex = &self.excludes;
        self.index.offloadable.retain(|k, _| ex.is_excluded(k));
        let mut replay = narrowed && self.index.initial_done && self.index.last_seq > 0;
        // Лимит скачивания подняли или сняли: пропущенное может стать нужным.
        let (old, new) = (self.index.max_download, self.cfg.max_download_size);
        if old > 0 && (new == 0 || new > old) && !self.index.skipped.is_empty() {
            replay |= self.index.last_seq > 0;
            self.index.skipped.clear();
        }
        self.index.max_download = new;
        if replay {
            self.index.last_seq = 0;
        }
        self.index.excludes = Some(lines);
        replay
    }

    /// Запись не скачивается из-за лимита: её здесь нет (ни файла, ни записи в
    /// индексе, в том числе по старому пути переименования), это файл, и он больше
    /// лимита. Всё, что уже есть локально, синхронизируется как обычно.
    pub fn over_download_limit(&self, key: &str, e: &pb::Entry) -> bool {
        let limit = self.index.max_download;
        limit > 0
            && !e.folder
            && !e.deleted
            && e.size > limit
            && !self.index.files.contains_key(key)
            && e.renamed_from
                .as_ref()
                .and_then(|p| self.decode_path(p))
                .is_none_or(|from| !self.index.files.contains_key(from.as_str()))
    }

    /// Убрать из индекса исключённые пути: ни удаления на сервер, ни отмены там —
    /// синк их просто больше не видит. Незавершённые передачи отменяются.
    pub fn forget_excluded(&mut self) -> Vec<String> {
        let gone: Vec<String> = self
            .index
            .files
            .keys()
            .filter(|k| self.is_excluded(k))
            .cloned()
            .collect();
        for k in &gone {
            if let Some(f) = self.index.files.remove(k) {
                // Совпадает с сервером — локальную копию можно удалить без потерь.
                if !f.folder
                    && f.base_rev > 0
                    && f.clean()
                    && let Some(l) = f.local
                {
                    self.index.offloadable.insert(k.clone(), l);
                }
                if let Some(crate::index::Transfer::Download { temp, .. }) = f.transfer {
                    self.stale_temps.push(temp);
                }
            }
            self.dirty.remove(k);
        }
        if !gone.is_empty() {
            let ex = &self.excludes;
            self.index
                .conflicts
                .retain(|c| !ex.is_excluded(&c.path) && !ex.is_excluded(&c.copy));
        }
        gone
    }

    /// Режим «только чтение» из `VaultState` (`None` — узнали по 403).
    pub fn set_read_only(&mut self, from_state: Option<bool>) {
        match from_state {
            Some(v) => self.read_only = v || self.read_only_forced,
            None => {
                self.read_only_forced = true;
                self.read_only = true;
            }
        }
        self.status.read_only = self.read_only;
    }

    /// Запомнить `Retry-After` из ответа 429.
    pub fn rate_limit(&mut self, now: i64, retry_after_s: u64) {
        let until = now.saturating_add(
            i64::try_from(retry_after_s.saturating_mul(1000)).unwrap_or(i64::MAX / 2),
        );
        self.rate_limited_until = Some(self.rate_limited_until.map_or(until, |u| u.max(until)));
        self.status.retry_at = until;
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
    pub fn encode_path(&self, p: &VaultPath) -> SyncResult<pb::Path> {
        match self.content_keys()? {
            Some(k) => Ok(k.encrypt_path(p)),
            None => Ok(p.to_proto()),
        }
    }

    /// Путь с сервера → локальный. Записи «не того» режима (остатки миграции)
    /// пропускаются.
    pub fn decode_path(&self, p: &pb::Path) -> Option<VaultPath> {
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

/// Правила в том виде, в каком они сохраняются в индексе (жёсткие — с пометкой).
fn rule_lines(cfg: &EngineConfig) -> Vec<String> {
    cfg.excludes
        .iter()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .chain(
            cfg.hard_excludes
                .iter()
                .map(|h| format!("hard:{}", h.trim())),
        )
        .collect()
}

/// Что-то могло стать включённым: убрано правило (исключение или жёсткое) или
/// добавлено `!`-правило. Перестановка тоже меняет смысл «последнего правила» —
/// считается сужением, повтор журнала безопасен.
fn rules_narrowed(old: &[String], new: &[String]) -> bool {
    old.iter().any(|l| !new.contains(l))
        || new.iter().any(|l| l.starts_with('!') && !old.contains(l))
        || {
            let common_old: Vec<&String> = old.iter().filter(|l| new.contains(l)).collect();
            let common_new: Vec<&String> = new.iter().filter(|l| old.contains(l)).collect();
            common_old != common_new
        }
}

pub(crate) fn build_excludes(cfg: &EngineConfig) -> Excludes {
    let mut e = Excludes::new(&cfg.excludes);
    for h in &cfg.hard_excludes {
        e.push_hard(h);
    }
    e
}

/// Суффикс временных файлов атомарной записи (исключается всегда).
pub const TEMP_SUFFIX: &str = ".selfsync-tmp";

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

    /// Путь, которого ФС устройства не может хранить (`cfg.name_rules`): сообщает
    /// о нём один раз за сессию и возвращает `true`. Сообщается часть пути до
    /// недопустимого сегмента — одна на папку, а не на каждый файл в ней.
    pub fn unsupported_name(&self, key: &str) -> bool {
        let Some(bad) = self.with(|s| {
            let rules = s.cfg.name_rules;
            VaultPath::parse(key).ok().and_then(|p| rules.rejects(&p))
        }) else {
            return false;
        };
        let bad = bad.as_str().to_owned();
        if self.with_mut(|s| s.notified.insert(format!("name:{bad}"))) {
            self.log(
                LogLevel::Warn,
                format!("{bad}: имя недопустимо на этом устройстве — файл здесь не создаётся"),
            );
            self.notify(Notice::UnsupportedName { path: bad });
        }
        true
    }

    /// Запомнить минорную версию протокола сервера из ответа. Заголовок есть — его
    /// минорная; нет в успешном ответе — сервер 0.1.x (`1.0`). Ошибка без заголовка
    /// могла прийти от прокси, а не от сервера: по ней не судим.
    fn note_server_proto(&self, status: u16, headers: &[(String, String)]) {
        let minor = match headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_PROTO))
        {
            Some((_, v)) => match parse_proto(v) {
                Some((PROTO_VERSION, m)) => m,
                _ => return,
            },
            None if (200..300).contains(&status) => 0,
            None => return,
        };
        if self.with(|s| s.status.server_minor) != Some(minor) {
            self.set_status(|st| st.server_minor = Some(minor));
        }
    }

    pub async fn http(
        &self,
        mut req: HttpRequest,
    ) -> SyncResult<(u16, Vec<(String, String)>, Vec<u8>)> {
        let client = self.with(|s| s.cfg.client.clone());
        if !client.is_empty() {
            req.headers.push((HEADER_CLIENT.to_owned(), client));
        }
        match self.hub.submit(|id| Action::Http { id, req }).await {
            IoResult::Http {
                status,
                headers,
                body,
            } => {
                self.note_server_proto(status, &headers);
                Ok((status, headers, body))
            }
            IoResult::Failed { message } => Err(SyncError::Network(message)),
            other => Err(SyncError::Io(format!(
                "неожиданный ответ на HTTP: {other:?}"
            ))),
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
    pub async fn read(
        &self,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> SyncResult<Option<Vec<u8>>> {
        let path = path.to_owned();
        match self
            .hub
            .submit(|id| Action::Read {
                id,
                path,
                offset,
                len,
            })
            .await
        {
            IoResult::Data { data } => Ok(Some(data)),
            IoResult::NotFound => Ok(None),
            other => Err(io_err("read", other)),
        }
    }

    /// Атомарная запись. `Ok(None)` — условие не выполнено (файл изменился).
    /// Синк изменил локальный путь (для `Notice::Synced`). `mkdir` — «убедиться,
    /// что есть», поэтому новые папки отмечает вызывающий.
    pub(crate) fn touched(&self, path: &str) {
        self.with_mut(|s| {
            s.pulled.insert(path.to_owned());
        });
    }

    pub async fn write(
        &self,
        path: &str,
        data: Vec<u8>,
        expect: Expect,
    ) -> SyncResult<Option<FileMeta>> {
        let r = self.write_inner(path, data, expect).await;
        if matches!(r, Ok(Some(_))) {
            self.touched(path);
        }
        r
    }

    async fn write_inner(
        &self,
        path: &str,
        data: Vec<u8>,
        expect: Expect,
    ) -> SyncResult<Option<FileMeta>> {
        let p = path.to_owned();
        match self
            .hub
            .submit(|id| Action::Write {
                id,
                path: p,
                data,
                expect,
            })
            .await
        {
            IoResult::Stat { meta } => Ok(meta.or_else(|| {
                Some(FileMeta {
                    path: path.to_owned(),
                    size: 0,
                    mtime: 0,
                    dir: false,
                })
            })),
            IoResult::Done => Ok(Some(FileMeta {
                path: path.to_owned(),
                size: 0,
                mtime: 0,
                dir: false,
            })),
            IoResult::Precondition => Ok(None),
            other => Err(io_err("write", other)),
        }
    }

    pub async fn write_temp(&self, temp: &str, offset: u64, data: Vec<u8>) -> SyncResult<()> {
        let temp = temp.to_owned();
        match self
            .hub
            .submit(|id| Action::WriteTemp {
                id,
                temp,
                offset,
                data,
            })
            .await
        {
            IoResult::Done => Ok(()),
            other => Err(io_err("write_temp", other)),
        }
    }

    pub async fn read_temp(
        &self,
        temp: &str,
        offset: u64,
        len: u64,
    ) -> SyncResult<Option<Vec<u8>>> {
        let temp = temp.to_owned();
        match self
            .hub
            .submit(|id| Action::ReadTemp {
                id,
                temp,
                offset,
                len,
            })
            .await
        {
            IoResult::Data { data } => Ok(Some(data)),
            IoResult::NotFound => Ok(None),
            other => Err(io_err("read_temp", other)),
        }
    }

    pub async fn commit_temp(
        &self,
        temp: &str,
        path: &str,
        expect: Expect,
    ) -> SyncResult<Option<FileMeta>> {
        let r = self.commit_temp_inner(temp, path, expect).await;
        if matches!(r, Ok(Some(_))) {
            self.touched(path);
        }
        r
    }

    async fn commit_temp_inner(
        &self,
        temp: &str,
        path: &str,
        expect: Expect,
    ) -> SyncResult<Option<FileMeta>> {
        let (t, p) = (temp.to_owned(), path.to_owned());
        match self
            .hub
            .submit(|id| Action::CommitTemp {
                id,
                temp: t,
                path: p,
                expect,
            })
            .await
        {
            IoResult::Stat { meta } => Ok(meta.or_else(|| {
                Some(FileMeta {
                    path: path.to_owned(),
                    size: 0,
                    mtime: 0,
                    dir: false,
                })
            })),
            IoResult::Done => Ok(Some(FileMeta {
                path: path.to_owned(),
                size: 0,
                mtime: 0,
                dir: false,
            })),
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

    /// Удалить насовсем. `Ok(false)` — файл изменился (условие не выполнено).
    pub async fn delete(&self, path: &str, expect: Expect) -> SyncResult<bool> {
        let key = path.to_owned();
        match self
            .hub
            .submit(|id| Action::Delete {
                id,
                path: key,
                expect,
            })
            .await
        {
            IoResult::Done | IoResult::NotFound => Ok(true),
            IoResult::Precondition => Ok(false),
            other => Err(io_err("delete", other)),
        }
    }

    /// В корзину. `Ok(false)` — файл изменился (условие не выполнено).
    pub async fn trash(&self, path: &str, expect: Expect) -> SyncResult<bool> {
        let key = path.to_owned();
        match self
            .hub
            .submit(|id| Action::Trash {
                id,
                path: key,
                expect,
            })
            .await
        {
            IoResult::Done => {
                self.touched(path);
                Ok(true)
            }
            IoResult::NotFound => Ok(true),
            IoResult::Precondition => Ok(false),
            other => Err(io_err("trash", other)),
        }
    }

    /// Переименование. `Ok(None)` — назначение занято или источника нет.
    pub async fn rename(&self, from: &str, to: &str) -> SyncResult<Option<FileMeta>> {
        let r = self.rename_inner(from, to).await;
        if matches!(r, Ok(Some(_))) {
            self.touched(from);
            self.touched(to);
        }
        r
    }

    /// Свободное временное имя `{base}.{tag}.selfsync-tmp` для двухшагового
    /// переименования. Занято (в том числе файлом, отличающимся только регистром) —
    /// `{base}.{tag}-N.selfsync-tmp`.
    pub async fn free_temp(&self, base: &str, tag: &str) -> SyncResult<String> {
        let mut name = format!("{base}.{tag}{TEMP_SUFFIX}");
        for n in 2..100 {
            if self.stat(&name).await?.is_none() {
                break;
            }
            name = format!("{base}.{tag}-{n}{TEMP_SUFFIX}");
        }
        Ok(name)
    }

    /// Двухшаговое переименование через временное имя (смена регистра или
    /// нормализации — для ФС это «то же самое» имя). `false` — не удалось, файл
    /// остался под старым именем (или временным, если откат тоже не удался —
    /// его доведёт `recover_temp`).
    pub async fn rename_via_temp(&self, from: &str, to: &str, tag: &str) -> SyncResult<bool> {
        let tmp = self.free_temp(to, tag).await?;
        if self.rename(from, &tmp).await?.is_none() {
            return Ok(false);
        }
        match self.rename(&tmp, to).await {
            Ok(Some(_)) => Ok(true),
            r => {
                let _ = self.rename(&tmp, from).await;
                r.map(|_| false)
            }
        }
    }

    async fn rename_inner(&self, from: &str, to: &str) -> SyncResult<Option<FileMeta>> {
        let (f, t) = (from.to_owned(), to.to_owned());
        match self
            .hub
            .submit(|id| Action::Rename { id, from: f, to: t })
            .await
        {
            IoResult::Stat { meta } => Ok(meta.or_else(|| {
                Some(FileMeta {
                    path: to.to_owned(),
                    size: 0,
                    mtime: 0,
                    dir: false,
                })
            })),
            IoResult::Done => Ok(Some(FileMeta {
                path: to.to_owned(),
                size: 0,
                mtime: 0,
                dir: false,
            })),
            IoResult::Precondition | IoResult::NotFound => Ok(None),
            other => Err(io_err("rename", other)),
        }
    }

    pub async fn mkdir(&self, path: &str) -> SyncResult<()> {
        let key = path.to_owned();
        match self.hub.submit(|id| Action::Mkdir { id, path: key }).await {
            IoResult::Done | IoResult::Stat { .. } => Ok(()),
            other => Err(io_err("mkdir", other)),
        }
    }

    /// Удалить пустую папку. `Ok(false)` — не пуста.
    pub async fn rmdir(&self, path: &str) -> SyncResult<bool> {
        let key = path.to_owned();
        match self.hub.submit(|id| Action::Rmdir { id, path: key }).await {
            IoResult::Done => {
                self.touched(path);
                Ok(true)
            }
            IoResult::NotFound => Ok(true),
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
            let _ = self
                .hub
                .submit(|id| Action::CacheWrite { id, key, data })
                .await;
        }
        for e in evict {
            let key = e.to_hex();
            let _ = self.hub.submit(|id| Action::CacheDelete { id, key }).await;
        }
    }

    /// Запомнить `VaultState` из ответа сервера; статус — только если изменилось
    /// состояние хранилища.
    pub fn note_server_state(&self, vs: &pb::VaultState) {
        let storage = StorageHealth::of(vs);
        let changed = self.with_mut(|s| {
            s.server_state = Some(*vs);
            s.status.storage != storage
        });
        if changed {
            self.set_status(|st| st.storage = storage);
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
