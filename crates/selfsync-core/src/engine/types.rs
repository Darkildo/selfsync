//! Внешний интерфейс движка: события, действия, результаты ввода-вывода.
//!
//! Все типы сериализуемы (serde): плагин получает их через WASM как JS-объекты,
//! байты — как `Uint8Array`.

use serde::{Deserialize, Serialize};

use crate::crypto::KdfParams;
use crate::path::NameRules;

/// Настройки клиента.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EngineConfig {
    /// Имя устройства — для имён конфликтных копий.
    pub device_name: String,
    /// Кто это (`plugin/0.3.0`, `cli/0.3.0`): заголовок `x-selfsync-client`, сервер
    /// показывает его в списке устройств. Пусто — не отправлять.
    pub client: String,
    /// Пользовательские исключения (см. [`crate::exclude`]).
    pub excludes: Vec<String>,
    /// Исключения, которые пользователь не может снять (каталог плагина).
    pub hard_excludes: Vec<String>,
    /// Пути (шаблоны как у исключений), которые не сливаются построчно: при
    /// расхождении побеждает более поздняя mtime, копия рядом не создаётся
    /// (файлы настроек Obsidian: diff3 дал бы невалидный JSON).
    pub no_merge: Vec<String>,
    /// Регистронезависимая ФС (Windows, macOS, iOS, Android-хранилище).
    pub case_insensitive: bool,
    /// Какие имена может хранить ФС устройства: записи с другими не скачиваются.
    pub name_rules: NameRules,
    /// Пауза после последней правки перед синком, мс.
    pub debounce_ms: u64,
    /// Интервал опроса при активности, мс.
    pub poll_active_ms: u64,
    /// Максимальный интервал опроса в простое, мс.
    pub poll_idle_max_ms: u64,
    /// Сервер работает постоянно: использовать long-poll `/v1/wait`.
    pub use_wait: bool,
    /// Полное сканирование vault'а не реже, мс (ловит правки вне событий Obsidian).
    pub full_scan_ms: u64,
    /// Лимит размера файла на клиенте.
    pub max_file_size: u64,
    /// Не скачивать серверные файлы больше этого размера, если их здесь нет
    /// (облегчённое устройство); 0 — без лимита. Свои файлы отправляются как обычно.
    pub max_download_size: u64,
    /// Параметры KDF для новых записей ключа (в тестах — облегчённые).
    #[serde(skip, default = "default_kdf")]
    pub kdf: KdfParams,
    /// Лимит кэша базовых версий, байт.
    pub base_cache_bytes: u64,
    /// Смещение часового пояса, минуты (для имён конфликтных копий).
    pub tz_offset_min: i32,
}

fn default_kdf() -> KdfParams {
    KdfParams::DEFAULT
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            device_name: "device".to_owned(),
            client: String::new(),
            excludes: Vec::new(),
            hard_excludes: Vec::new(),
            no_merge: Vec::new(),
            case_insensitive: false,
            name_rules: NameRules::Any,
            debounce_ms: 2500,
            poll_active_ms: 15_000,
            poll_idle_max_ms: 300_000,
            use_wait: false,
            full_scan_ms: 10 * 60_000,
            max_file_size: 512 * 1024 * 1024,
            max_download_size: 0,
            kdf: KdfParams::DEFAULT,
            base_cache_bytes: 32 * 1024 * 1024,
            tz_offset_min: 0,
        }
    }
}

/// Метаданные локального файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    #[serde(default)]
    pub dir: bool,
}

/// Условие атомарной записи: файл не должен был измениться с момента чтения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Expect {
    /// Без условия.
    Any,
    /// Файла быть не должно.
    Absent,
    /// Размер и mtime совпадают с наблюдёнными.
    Stat { size: u64, mtime: i64 },
}

/// HTTP-запрос. Базовый адрес сервера и `Authorization` добавляет исполнитель
/// (если `auth`): токен в ядро не попадает.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    pub method: String,
    /// Путь с query: `/v1/changes?since=0`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
    pub auth: bool,
    pub timeout_ms: u64,
}

/// Действие, которое должен выполнить исполнитель.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// HTTP-запрос → `IoResult::Http` или `IoResult::Failed`.
    Http {
        id: u64,
        req: HttpRequest,
    },
    /// Список всех файлов и папок vault'а → `IoResult::Listing`.
    List {
        id: u64,
    },
    /// Метаданные пути → `IoResult::Stat`.
    Stat {
        id: u64,
        path: String,
    },
    /// Прочитать файл (целиком, если `len` = None) → `IoResult::Data` / `NotFound`.
    Read {
        id: u64,
        path: String,
        offset: u64,
        len: Option<u64>,
    },
    /// Атомарно записать файл целиком → `IoResult::Stat` нового файла или
    /// `Precondition`. Если ФС не умеет атомарно заменять файл, исполнитель пишет
    /// `X.selfsync-tmp`, переименовывает его в `X.commit.selfsync-tmp` (записан
    /// целиком), удаляет `X` и переименовывает в `X`: прерванную запись ядро доведёт
    /// при следующем сканировании.
    Write {
        id: u64,
        path: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        expect: Expect,
    },
    /// Дописать данные во временный файл скачивания (каталог плагина) → `Done`.
    WriteTemp {
        id: u64,
        temp: String,
        offset: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Прочитать временный файл → `Data`.
    ReadTemp {
        id: u64,
        temp: String,
        offset: u64,
        len: u64,
    },
    /// Атомарно перенести временный файл на место → `Stat` или `Precondition`.
    CommitTemp {
        id: u64,
        temp: String,
        path: String,
        expect: Expect,
    },
    /// Удалить временный файл → `Done`.
    DeleteTemp {
        id: u64,
        temp: String,
    },
    /// Убрать файл в корзину Obsidian (не стирать) → `Done` / `Precondition` / `NotFound`.
    /// Файла нет — `NotFound` при любом `expect` (удалять нечего); `Precondition` —
    /// только если файл есть, но изменился.
    Trash {
        id: u64,
        path: String,
        expect: Expect,
    },
    /// Удалить файл насовсем (освободить место; копия есть на сервере) → `Done` /
    /// `Precondition` (файл изменился) / `NotFound`. Только для исключённых путей,
    /// совпадавших с сервером (`RemoveLocal`).
    Delete {
        id: u64,
        path: String,
        expect: Expect,
    },
    /// Переименовать → `Stat` / `Precondition` (назначение занято) / `NotFound`.
    Rename {
        id: u64,
        from: String,
        to: String,
    },
    /// Создать папку (с родителями) → `Done`.
    Mkdir {
        id: u64,
        path: String,
    },
    /// Удалить пустую папку → `Done` / `Precondition` (не пуста) / `NotFound`.
    Rmdir {
        id: u64,
        path: String,
    },
    /// Сохранить снимок индекса (атомарно) → `Done`.
    SaveIndex {
        id: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Кэш базовых версий (каталог плагина).
    CacheRead {
        id: u64,
        key: String,
    },
    CacheWrite {
        id: u64,
        key: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    CacheDelete {
        id: u64,
        key: String,
    },
    /// Вызвать `Tick` не позже этого момента (мс). Ответа не требует.
    Wake {
        at: i64,
    },
    /// Новый статус для статус-бара. Ответа не требует.
    Status {
        status: SyncStatus,
    },
    /// Уведомление пользователю. Ответа не требует.
    Notify {
        notice: Notice,
    },
    /// Строка в лог плагина. Ответа не требует.
    Log {
        level: LogLevel,
        message: String,
    },
    /// Запомнить (или забыть) мастер-ключ на устройстве по выбору пользователя.
    RememberKey {
        #[serde(with = "serde_bytes")]
        key: Vec<u8>,
    },
    ForgetKey,
    /// Результат команды интерфейса.
    UiResult {
        req: u64,
        result: UiResult,
    },
}

impl Action {
    /// Id действия, требующего ответа.
    pub fn id(&self) -> Option<u64> {
        match self {
            Action::Http { id, .. }
            | Action::List { id }
            | Action::Stat { id, .. }
            | Action::Read { id, .. }
            | Action::Write { id, .. }
            | Action::WriteTemp { id, .. }
            | Action::ReadTemp { id, .. }
            | Action::CommitTemp { id, .. }
            | Action::DeleteTemp { id, .. }
            | Action::Trash { id, .. }
            | Action::Delete { id, .. }
            | Action::Rename { id, .. }
            | Action::Mkdir { id, .. }
            | Action::Rmdir { id, .. }
            | Action::SaveIndex { id, .. }
            | Action::CacheRead { id, .. }
            | Action::CacheWrite { id, .. }
            | Action::CacheDelete { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// Результат действия.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum IoResult {
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Listing {
        files: Vec<FileMeta>,
    },
    Stat {
        meta: Option<FileMeta>,
    },
    Data {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Done,
    NotFound,
    /// Условие записи не выполнено (файл изменился / назначение занято / папка не пуста).
    Precondition,
    /// Сбой (сеть, ФС). Для HTTP — запрос мог как дойти, так и не дойти до сервера.
    Failed {
        message: String,
    },
}

/// Входное событие.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// Запуск. `key` — запомненный на устройстве мастер-ключ (если пользователь разрешил).
    Start {
        #[serde(default, with = "serde_bytes")]
        key: Option<Vec<u8>>,
    },
    /// Таймер (в ответ на `Wake` или периодически).
    Tick,
    /// Синк сейчас (клик по статус-бару, команда).
    SyncNow,
    /// Файл создан или изменён.
    Changed {
        path: String,
    },
    /// Файл или папка удалены.
    Deleted {
        path: String,
    },
    /// Переименование.
    Renamed {
        from: String,
        to: String,
    },
    /// Приложение на переднем плане / ушло в фон.
    Visible,
    Hidden,
    /// Ответ на действие.
    Done {
        id: u64,
        result: IoResult,
    },
    /// Пароль шифрования.
    Password {
        password: String,
        remember: bool,
    },
    /// Включить шифрование существующего vault'а.
    EnableEncryption {
        password: String,
        remember: bool,
        /// Создать и фразу восстановления (запись формата 2: клиенты 0.1.x её не
        /// откроют). Фраза приходит один раз — `Notice::RecoveryPhrase`.
        #[serde(default)]
        recovery: bool,
    },
    /// Сменить пароль (мастер-ключ прежний).
    ChangePassword {
        old: String,
        new: String,
    },
    /// Решение по конфликту.
    Resolve {
        id: u64,
        choice: ConflictChoice,
    },
    /// Команда интерфейса (ответ — `Action::UiResult` с тем же `req`).
    Command {
        req: u64,
        command: UiCommand,
    },
    /// Новые настройки.
    Configure {
        config: EngineConfig,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictChoice {
    /// Оставить обе версии (ничего не делать, запись конфликта убрать).
    KeepBoth,
    /// Оставить свою: содержимое копии — на место, копия в корзину.
    KeepMine,
    /// Оставить серверную: копия в корзину.
    KeepServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SyncState {
    #[default]
    Idle,
    Syncing,
    /// Сервер недоступен.
    Offline,
    Error,
    /// Нужен пароль шифрования.
    NeedPassword,
    /// Синк остановлен до вмешательства (неверный пароль, отозван токен, ...).
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub state: SyncState,
    /// Машинный код причины (для локализации в UI).
    pub reason: Option<String>,
    pub pending: u32,
    pub done: u32,
    pub total: u32,
    pub last_sync: i64,
    pub conflicts: u32,
    pub encrypted: bool,
    /// Сервер ограничил попытки (`reason == "rate_limited"`): следующий запрос не
    /// раньше этого момента, мс. `0` — ограничения нет.
    pub retry_at: i64,
    /// Устройство только для чтения: свои правки не отправляются (`pending` — сколько
    /// их накопилось).
    pub read_only: bool,
    /// У записи ключа есть фраза восстановления.
    pub recovery: bool,
    /// Минорная версия протокола сервера (`0` — сервер 0.1.x); `None` — ещё ни
    /// одного ответа. Меньше [`selfsync_proto::PROTO_MINOR`] — сервер старее клиента
    /// и часть возможностей недоступна.
    pub server_minor: Option<u32>,
    /// Состояние хранилища на сервере по последнему `selfsync verify`.
    #[serde(default)]
    pub storage: StorageHealth,
    /// Квота vault'а на сервере и файлы, которые ждут места.
    #[serde(default)]
    pub quota: QuotaStatus,
}

/// Итог шага проверки соединения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagLevel {
    Ok,
    Warn,
    Fail,
    /// Не проверялось (неприменимо или помешало другое).
    Skip,
}

/// Шаг проверки соединения (`UiCommand::Diagnose`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagCheck {
    /// `server`, `proxy`, `version`, `clock`, `token`, `body`, `longpoll`.
    pub step: String,
    pub level: DiagLevel,
    /// Машинный код итога для текста интерфейса: `ok`, `tls`, `dns`, `refused`,
    /// `timeout`, `network`, `not_selfsync`, `headers_stripped`, `proto_major`,
    /// `server_old`, `skew`, `no_date`, `read_only`, `unauthorized`, `client_old`,
    /// `rate_limited`, `body_limit`, `server_limit`, `quota`, `unexpected`, `off`, `changed`,
    /// `no_longpoll`, `failed`, `error`.
    pub code: String,
    /// Подробность: сообщение сети, версия сервера, расхождение часов в секундах.
    pub detail: String,
}

/// Квота vault'а (протокол 1.3; у старого сервера всё нули).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaStatus {
    /// Занято на сервере: текущие версии, история и корзина.
    pub used: u64,
    /// Предупреждение; `0` — без порога.
    pub soft: u64,
    /// Новые файлы не принимаются; `0` — без порога.
    pub hard: u64,
    /// Файлы, которые сервер не принял: места нет. Отправятся, когда оно появится.
    pub held: u32,
}

/// Состояние хранилища на сервере (итог `selfsync verify`, протокол 1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageHealth {
    /// Когда сервер последний раз проверял vault, мс; `0` — не проверял или сервер
    /// старее 0.4.0.
    pub verified_at: i64,
    /// Блобов повреждено или потеряно на сервере; ждут копии с устройств.
    pub damaged: u32,
    /// Повреждение, которое устройства не исправят (база vault'а, запись ключа):
    /// нужен админ сервера.
    pub server_problem: bool,
}

impl StorageHealth {
    pub fn of(vs: &selfsync_proto::v1::VaultState) -> StorageHealth {
        StorageHealth {
            verified_at: vs.verified_at,
            damaged: vs.blobs_wanted,
            server_problem: vs.storage_problem,
        }
    }

    pub fn damaged(&self) -> bool {
        self.damaged > 0 || self.server_problem
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Уведомления: структурированные, текст формирует UI (ru/en).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Notice {
    /// Пересекающиеся правки: обе версии сохранены, нужен выбор.
    Conflict {
        id: u64,
        path: String,
        copy: String,
    },
    /// Локальный файл при первичной загрузке отличался — сохранён копией.
    LocalCopySaved {
        path: String,
        copy: String,
    },
    /// Файл, удалённый на другом устройстве, возвращён: здесь его правили.
    RestoredEdited {
        path: String,
    },
    /// Локальное удаление отменено: файл правили на другом устройстве.
    RestoredRemote {
        path: String,
    },
    /// Файл переименован на другом устройстве, локальные правки перенесены.
    FollowedRename {
        from: String,
        to: String,
    },
    /// Сервер отказался принять файл.
    Rejected {
        path: String,
        code: String,
    },
    /// Имя отличается только регистром от существующего — пропущено.
    CaseCollision {
        path: String,
        existing: String,
    },
    /// Файл больше лимита.
    /// Файлы больше лимита скачивания не скачаны (раз за сессию).
    Skipped {
        count: u32,
    },
    TooLarge {
        path: String,
        size: u64,
    },
    /// Цикл синка что-то изменил: `pulled` — пути, тронутые синком здесь (пришло
    /// с сервера, удалено, переименовано, копии конфликтов), `pushed` —
    /// отправленные. Пути открытые (для зашифрованного vault'а — расшифрованные),
    /// по возрастанию. Только если хоть один список не пуст.
    Synced {
        pulled: Vec<String>,
        pushed: Vec<String>,
    },
    /// Шифрование включено вместе с фразой восстановления: показать её один раз.
    RecoveryPhrase {
        words: Phrase,
    },
    /// Устройство только для чтения, а локальные правки есть: на сервер они не
    /// уйдут. Один раз за сессию.
    ReadOnly {
        pending: u32,
    },
    /// Место vault'а на сервере закончилось: `held` файлов ждут, пока оно появится.
    /// Один раз, пока ждут.
    QuotaFull {
        held: u32,
    },
    /// Занято больше мягкого порога квоты. Один раз, пока не станет меньше.
    QuotaSoft {
        used: u64,
        soft: u64,
    },
    /// Проверка на сервере нашла повреждения. Один раз, пока они не исчезнут.
    StorageDamaged {
        damaged: u32,
        server_problem: bool,
    },
    /// Имя недопустимо на этом устройстве (`?` на Android и т.п.): файл или папка
    /// с другого устройства не скачаны. `path` — до недопустимого сегмента.
    UnsupportedName {
        path: String,
    },
    /// Имя отправленного отсюда файла не создать на Windows и Android: туда он не
    /// попадёт. `path` — до недопустимого сегмента.
    NonPortableName {
        path: String,
    },
    /// Неверный пароль: синк остановлен.
    WrongPassword,
    /// Нужен пароль (vault зашифрован).
    NeedPassword,
    /// На другом устройстве включают шифрование.
    EncryptionStarted,
    /// Ключ шифрования меняет другое устройство: запись приостановлена.
    RekeyStarted,
    /// Смена ключа завершена (на этом устройстве).
    RekeyDone,
    /// Миграция на шифрование: прогресс и завершение.
    MigrationProgress {
        done: u32,
        total: u32,
    },
    EncryptionEnabled,
    PasswordChanged,
    /// Токен не принят: устройство отозвано.
    Unauthorized,
    /// Версия протокола сервера не поддерживается.
    ProtocolUnsupported {
        supported: u32,
    },
    /// Сервер стал «моложе» клиента (восстановлен из бэкапа) — полная сверка.
    ServerRewound,
    /// Индекс повреждён — начата безопасная первичная сверка.
    IndexReset,
    /// Сервер не зашифрован, а локально vault зашифрован: синк остановлен.
    EncryptionMismatch,
    /// Прочая ошибка.
    Error {
        code: String,
        message: String,
    },
}

/// Команды интерфейса.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UiCommand {
    ListDeleted,
    RestoreDeleted {
        paths: Vec<String>,
    },
    PurgeDeleted {
        paths: Vec<String>,
    },
    History {
        path: String,
    },
    RestoreRevision {
        path: String,
        rev: u64,
    },
    /// Стереть ревизии файла старше `before_rev` (текущая и последняя живая
    /// остаются), освободить место. Нужны сервер 1.3+ и право записи. Ответ —
    /// `Restored { count }` (сколько стёрто).
    PurgeHistory {
        path: String,
        before_rev: u64,
    },
    Devices,
    RevokeDevice {
        id: u32,
    },
    CreateJoin {
        name: String,
        /// Подключить новое устройство только для чтения.
        #[serde(default)]
        read_only: bool,
    },
    Redeem {
        code: String,
        name: String,
    },
    GetRetention,
    SetRetention {
        days: u32,
    },
    Stats,
    /// Список нерешённых конфликтов.
    Conflicts,
    /// Оценка пароля (биты) — совет, а не запрет.
    PasswordStrength {
        password: String,
    },
    /// Создать фразу восстановления (или новую вместо прежней — та перестаёт
    /// работать). Ответ — `RecoveryPhrase`, один раз.
    CreateRecovery {
        password: String,
    },
    /// Убрать фразу восстановления.
    RemoveRecovery {
        password: String,
    },
    /// Забыли пароль: раскрыть ключ фразой и задать новый пароль. Фраза остаётся
    /// действительной.
    Recover {
        phrase: String,
        new_password: String,
        remember: bool,
    },
    /// Сменить ключ шифрования (после отзыва устройства): новый мастер-ключ под тем
    /// же паролем, перешифровка vault'а в ближайшем цикле. Нужны сервер 1.2+ и
    /// право записи.
    Rekey {
        password: String,
        #[serde(default)]
        remember: bool,
    },
    /// Проверка соединения по шагам (`Diagnosis`): адрес и TLS, прокси, версия,
    /// часы, токен, лимит тела запроса, long-poll.
    Diagnose,
    /// Файлы, не скачанные из-за лимита размера.
    Skipped,
    /// Скачать пропущенный файл несмотря на лимит (в ближайшем цикле синка); дальше
    /// он синхронизируется как обычно.
    Download {
        path: String,
    },
    /// Удалить локальные копии под исключённым путём, совпадающие с сервером
    /// (освободить место). Путь должен быть исключён уже применёнными правилами.
    RemoveLocal {
        path: String,
    },
}

/// Файл, не скачанный из-за лимита размера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedView {
    pub path: String,
    pub size: u64,
}

/// 24 слова фразы восстановления. `Debug` их не печатает: фраза не должна попасть
/// в журнал ни на одном устройстве.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Phrase(pub Vec<String>);

impl std::fmt::Debug for Phrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Phrase({} слов, скрыто)", self.0.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedView {
    pub path: String,
    pub deleted_at: i64,
    pub expires_at: i64,
    pub size: u64,
    pub device: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionView {
    pub rev: u64,
    pub size: u64,
    pub mtime: i64,
    pub seq: u64,
    pub device: u32,
    pub deleted: bool,
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub id: u32,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub revoked: bool,
    pub current: bool,
    pub read_only: bool,
    /// Клиент устройства (`plugin/0.3.0`); пусто — неизвестен (старее 0.3.0).
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UiResult {
    Ok,
    /// Фраза восстановления — показать один раз и забыть.
    RecoveryPhrase {
        words: Phrase,
    },
    Error {
        code: String,
        message: String,
        /// Для `rate_limited`: через сколько секунд можно повторить.
        #[serde(default)]
        retry_after: u64,
    },
    Deleted {
        items: Vec<DeletedView>,
    },
    Diagnosis {
        checks: Vec<DiagCheck>,
    },
    History {
        revisions: Vec<RevisionView>,
    },
    Devices {
        vault: String,
        devices: Vec<DeviceView>,
    },
    Join {
        code: String,
        url: String,
        expires_at: i64,
    },
    Token {
        token: String,
        vault: String,
        device_id: u32,
        device_name: String,
    },
    Retention {
        days: u32,
    },
    Stats {
        seq: u64,
        files: u64,
        folders: u64,
        deleted: u64,
        live_bytes: u64,
        stored_bytes: u64,
        devices: u32,
    },
    Conflicts {
        items: Vec<crate::index::ConflictRecord>,
    },
    Skipped {
        files: Vec<SkippedView>,
    },
    /// `RemoveLocal`: удалено файлов и сколько оставлено (их нет на сервере или они
    /// изменены здесь).
    Removed {
        removed: u32,
        kept: u32,
    },
    Strength {
        bits: u32,
    },
    Restored {
        count: u32,
    },
}
