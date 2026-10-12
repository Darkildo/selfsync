//! Цикл клиента: события (ответы исполнителя, таймер, слежение за папкой,
//! Ctrl-C) → ядро → действия → исполнитель. Ядро не `Send` и живёт в этой задаче,
//! ввод-вывод выполняется отдельными задачами tokio.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use selfsync_core::engine::{
    Action, ConflictChoice, Engine, EngineConfig, Event, LogLevel, Notice, SyncState, SyncStatus,
    UiCommand, UiResult,
};
use tokio::sync::mpsc;

use crate::config::{Config, STATE_DIR, write_private};
use crate::exec::Exec;
use crate::hook::{Batch, Hook, HookConfig};

pub struct Options {
    /// Один цикл синка и выход (cron, скрипты, тесты).
    pub once: bool,
    /// Следить за папкой (иначе — только опрос и полное сканирование).
    pub watch: bool,
    /// Пароль шифрования, если vault зашифрован.
    pub password: Option<String>,
    /// Запомнить ключ в `.selfsync/key` (0600), чтобы перезапуск не просил пароль.
    pub remember_key: bool,
    /// Включить шифрование vault'а паролем `password` (после первой синхронизации).
    pub enable_encryption: bool,
    /// Команда после каждого цикла, который что-то изменил (`run --exec`).
    pub hook: Option<HookConfig>,
    /// С `enable_encryption`: создать и фразу восстановления (печатается в stdout).
    pub recovery: bool,
}

/// Чем закончился запуск.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Synced,
    /// Сервер недоступен или ответил ошибкой (ядро повторило бы позже).
    Failed(String),
    NeedPassword,
    Blocked(String),
    Interrupted,
}

/// Причина состояния для человека. Для 429 — сколько ждать.
fn reason_text(s: &SyncStatus) -> String {
    match s.reason.as_deref() {
        Some("rate_limited") => {
            let secs = ((s.retry_at - now_ms()).max(0) + 999) / 1000;
            format!("сервер ограничил попытки, следующая через {secs} с")
        }
        Some(r) => r.to_owned(),
        None => String::new(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

struct Loop {
    engine: Engine,
    exec: Arc<Exec>,
    tx: mpsc::UnboundedSender<Event>,
    inflight: usize,
    wake: Option<i64>,
    status: SyncStatus,
    ui: HashMap<u64, UiResult>,
    key_path: PathBuf,
    hook: Option<Hook>,
}

impl Loop {
    fn new(
        cfg: EngineConfig,
        index: Option<&[u8]>,
        exec: Arc<Exec>,
        tx: mpsc::UnboundedSender<Event>,
    ) -> Loop {
        let key_path = exec.key_path();
        Loop {
            engine: Engine::new(cfg, index),
            exec,
            tx,
            inflight: 0,
            wake: None,
            status: SyncStatus::default(),
            ui: HashMap::new(),
            key_path,
            hook: None,
        }
    }

    fn feed(&mut self, ev: Event) {
        if matches!(ev, Event::Done { .. }) {
            self.inflight = self.inflight.saturating_sub(1);
        }
        for a in self.engine.handle(now_ms(), ev) {
            self.act(a);
        }
        // Wake ядро шлёт только при изменении момента — свериться с его желанием.
        if let Some(w) = self.engine.next_wake()
            && self.wake.is_none_or(|cur| w < cur)
        {
            self.wake = Some(w);
        }
    }

    fn act(&mut self, a: Action) {
        match a {
            Action::Wake { at } => self.wake = Some(at),
            Action::Status { status } => {
                if status.read_only != self.status.read_only {
                    // Для `status` без сети: режим знает только сервер.
                    let marker = self.key_path.with_file_name(READ_ONLY_MARKER);
                    if status.read_only {
                        let _ = std::fs::write(&marker, b"");
                    } else {
                        let _ = std::fs::remove_file(&marker);
                    }
                }
                if status.server_minor != self.status.server_minor
                    && let Some(m) = status.server_minor
                    && m < selfsync_core::proto::PROTO_MINOR
                {
                    tracing::warn!(
                        "сервер старее клиента (протокол {}.{m}, у клиента {}): обновите \
                         сервер до 0.2.1 или новее",
                        selfsync_core::proto::PROTO_VERSION,
                        selfsync_core::proto::proto_header()
                    );
                }
                if status.state != self.status.state || status.retry_at != self.status.retry_at {
                    tracing::info!("состояние: {:?} ({})", status.state, reason_text(&status));
                }
                self.status = status;
            }
            Action::Notify { notice } => {
                if let (Some(h), Notice::Synced { pulled, pushed }) = (&self.hook, &notice) {
                    h.send(Batch {
                        pulled: pulled.len() as u64,
                        pushed: pushed.len() as u64,
                        paths: pulled.iter().chain(pushed).cloned().collect(),
                    });
                }
                if let Notice::RecoveryPhrase { words } = &notice {
                    print_phrase(words);
                }
                notify_user(&notice);
            }
            Action::Log { level, message } => match level {
                LogLevel::Debug => tracing::debug!("{message}"),
                LogLevel::Info => tracing::info!("{message}"),
                LogLevel::Warn => tracing::warn!("{message}"),
                LogLevel::Error => tracing::error!("{message}"),
            },
            Action::RememberKey { key } => {
                if let Err(e) = write_private(&self.key_path, &key) {
                    tracing::warn!("ключ не сохранён: {e}");
                }
            }
            Action::ForgetKey => {
                let _ = std::fs::remove_file(&self.key_path);
            }
            Action::UiResult { req, result } => {
                self.ui.insert(req, result);
            }
            io => {
                let Some(id) = io.id() else { return };
                self.inflight += 1;
                let exec = Arc::clone(&self.exec);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = exec.perform(io).await;
                    let _ = tx.send(Event::Done { id, result });
                });
            }
        }
    }

    fn sleep(&self) -> Duration {
        self.wake.map_or(Duration::from_secs(3600), |at| {
            Duration::from_millis(u64::try_from(at - now_ms()).unwrap_or(0))
        })
    }

    fn settled(&self) -> bool {
        self.inflight == 0 && self.engine.is_idle()
    }
}

/// Фраза восстановления — в stdout (её можно перенаправить в файл), предупреждение
/// — в stderr. Больше нигде она не появляется.
pub fn print_phrase(words: &selfsync_core::engine::Phrase) {
    eprintln!(
        "Фраза восстановления (показывается один раз; запишите её и храните отдельно от \
         пароля — она раскрывает vault так же, как пароль):"
    );
    for (i, w) in words.0.iter().enumerate() {
        println!("{:2}. {w}", i + 1);
    }
}

/// Команда на движке папки без цикла синка: индекс и ключ загружены (запись
/// индекса и ключа идёт как обычно), сеть — только для самой команды. Для входа
/// по фразе: обычная команда сначала синхронизирует, а без пароля синк не начнётся.
pub async fn folder_command(dir: &Path, cfg: &Config, cmd: UiCommand) -> anyhow::Result<UiResult> {
    let exec = Arc::new(Exec::new(dir, &cfg.server, Some(cfg.token.clone()))?);
    let index = std::fs::read(exec.index_path()).ok();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut lp = Loop::new(cfg.engine(), index.as_deref(), exec, tx);
    lp.feed(Event::Command {
        req: 1,
        command: cmd,
    });
    loop {
        if lp.inflight == 0
            && let Some(r) = lp.ui.remove(&1)
        {
            return Ok(r);
        }
        match tokio::time::timeout(Duration::from_secs(300), rx.recv()).await {
            Ok(Some(ev)) => lp.feed(ev),
            _ => anyhow::bail!("сервер не ответил"),
        }
    }
}

/// Файл в `.selfsync/`: последний синк прошёл в режиме «только чтение».
pub const READ_ONLY_MARKER: &str = "read-only";

fn notify_user(n: &Notice) {
    match n {
        // Фраза уже напечатана в stdout; в журнал — только факт.
        Notice::RecoveryPhrase { .. } => tracing::info!("создана фраза восстановления"),
        Notice::Synced { pulled, pushed } => {
            tracing::info!("синк: пришло {}, отправлено {}", pulled.len(), pushed.len())
        }
        Notice::ReadOnly { pending } => tracing::warn!(
            "устройство только для чтения: локальные правки ({pending}) на сервер не отправляются"
        ),
        Notice::Conflict { path, copy, .. } => {
            tracing::warn!("конфликт: {path} — локальная версия сохранена как {copy}")
        }
        Notice::LocalCopySaved { path, copy } => {
            tracing::warn!("{path} отличался от серверного: локальная версия сохранена как {copy}")
        }
        Notice::Error { message, .. } => tracing::error!("{message}"),
        Notice::UnsupportedName { path } => {
            tracing::warn!("{path}: имя недопустимо на этой системе — сюда не синхронизируется")
        }
        Notice::NonPortableName { path } => {
            tracing::warn!("{path}: имя недопустимо на Windows и Android — туда файл не попадёт")
        }
        Notice::MigrationProgress { done, total } => {
            tracing::info!("шифрование vault'а: {done}/{total}")
        }
        other => tracing::info!("{other:?}"),
    }
}

/// Слежение за папкой: события ФС → события ядра (пути относительно корня).
fn watch(
    root: &Path,
    tx: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<notify::RecommendedWatcher> {
    let base = root.canonicalize()?;
    let rel = move |p: &Path| -> Option<String> {
        let r = p.strip_prefix(&base).ok()?.to_str()?.replace('\\', "/");
        (!r.is_empty() && !r.starts_with(STATE_DIR)).then_some(r)
    };
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(e) = res else { return };
        let paths: Vec<String> = e.paths.iter().filter_map(|p| rel(p)).collect();
        let send = |ev: Event| {
            let _ = tx.send(ev);
        };
        match e.kind {
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                send(Event::Renamed {
                    from: paths[0].clone(),
                    to: paths[1].clone(),
                })
            }
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                for p in paths {
                    send(Event::Deleted { path: p });
                }
            }
            EventKind::Access(_) => {}
            _ => {
                for p in paths {
                    send(Event::Changed { path: p });
                }
            }
        }
    })?;
    w.watch(root, RecursiveMode::Recursive)?;
    Ok(w)
}

/// Синхронизация папки: до Ctrl-C или (с `once`) до конца одного цикла. С `once`
/// хук, если задан, успевает выполниться до выхода; его код на исход не влияет.
pub async fn run(dir: &Path, cfg: &Config, opts: Options) -> anyhow::Result<Outcome> {
    let once = opts.once;
    let hook = opts.hook.clone().map(|h| Hook::start(dir, h));
    let end = run_loop(dir, cfg, opts, hook, None).await?;
    if once && let Some(h) = end.hook {
        h.finish().await;
    }
    Ok(end.outcome)
}

/// Команда для [`with_vault`].
#[derive(Debug, Clone)]
pub enum VaultCommand {
    /// Команда окна (история, корзина, откат...).
    Ui(UiCommand),
    /// Решение по конфликту.
    Resolve { id: u64, choice: ConflictChoice },
}

impl VaultCommand {
    /// Меняет файлы или сервер: после ответа нужен ещё один цикл синка.
    fn changes(&self) -> bool {
        matches!(
            self,
            VaultCommand::Resolve { .. }
                | VaultCommand::Ui(
                    UiCommand::RestoreDeleted { .. }
                        | UiCommand::PurgeDeleted { .. }
                        | UiCommand::RestoreRevision { .. }
                        | UiCommand::Download { .. }
                )
        )
    }
}

/// Команда на настоящем движке папки: индекс и ключ загружены, сначала цикл синка
/// (нужен ключ для имён зашифрованного vault'а и актуальный индекс), потом команда,
/// для меняющих — ещё один цикл. Без слежения за папкой. Исход — как у
/// `run --once`; ответ команды — `None`, если до неё не дошло (сеть, пароль).
pub async fn with_vault(
    dir: &Path,
    cfg: &Config,
    password: Option<String>,
    remember_key: bool,
    cmd: VaultCommand,
) -> anyhow::Result<(Outcome, Option<UiResult>)> {
    let opts = Options {
        once: true,
        watch: false,
        password,
        remember_key,
        enable_encryption: false,
        hook: None,
        recovery: false,
    };
    let end = run_loop(dir, cfg, opts, None, Some(cmd)).await?;
    Ok((end.outcome, end.result))
}

struct LoopEnd {
    outcome: Outcome,
    hook: Option<Hook>,
    result: Option<UiResult>,
}

/// Где команда `with_vault`.
enum Stage {
    /// Ждём первого цикла.
    Waiting(VaultCommand),
    /// Отправлена, ждём ответа (`Resolve` ответа не даёт — ждём простоя).
    Sent { changes: bool, ui: bool },
    /// Ответ есть, идёт цикл синка после неё.
    Syncing,
}

const COMMAND_REQ: u64 = 1;

async fn run_loop(
    dir: &Path,
    cfg: &Config,
    opts: Options,
    hook: Option<Hook>,
    command: Option<VaultCommand>,
) -> anyhow::Result<LoopEnd> {
    let exec = Arc::new(Exec::new(dir, &cfg.server, Some(cfg.token.clone()))?);
    let index = std::fs::read(exec.index_path()).ok();
    let key = std::fs::read(exec.key_path()).ok();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut lp = Loop::new(
        cfg.engine(),
        index.as_deref(),
        Arc::clone(&exec),
        tx.clone(),
    );
    lp.hook = hook;
    let _watcher = if opts.watch && !opts.once {
        Some(watch(dir, tx.clone())?)
    } else {
        None
    };
    let mut stage = command.map(Stage::Waiting);
    let mut result = None;
    let end = |lp: &mut Loop, outcome: Outcome, result: Option<UiResult>| LoopEnd {
        outcome,
        hook: lp.hook.take(),
        result,
    };

    lp.feed(Event::Start { key });
    if opts.enable_encryption {
        let password = opts
            .password
            .clone()
            .ok_or_else(|| anyhow::anyhow!("для шифрования нужен пароль"))?;
        lp.feed(Event::EnableEncryption {
            password,
            remember: opts.remember_key,
            recovery: opts.recovery,
        });
    }
    let mut password_sent = opts.enable_encryption;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => lp.feed(ev),
                None => return Ok(end(&mut lp, Outcome::Interrupted, result)),
            },
            () = tokio::time::sleep(lp.sleep()) => {
                lp.wake = None;
                lp.feed(Event::Tick);
            }
            _ = tokio::signal::ctrl_c() => {
                // Дать дописать начатое: индекс сохраняется после каждого файла.
                return Ok(end(&mut lp, Outcome::Interrupted, result));
            }
        }
        if lp.status.state == SyncState::NeedPassword && !password_sent {
            match &opts.password {
                Some(pw) => {
                    password_sent = true;
                    lp.feed(Event::Password {
                        password: pw.clone(),
                        remember: opts.remember_key,
                    });
                    lp.feed(Event::SyncNow);
                }
                None if opts.once => return Ok(end(&mut lp, Outcome::NeedPassword, result)),
                None => {}
            }
        }
        // Стадия команды может смениться без новых событий (ответ без сети) —
        // поэтому проверка повторяется, пока что-то меняется.
        'check: loop {
            if !(opts.once && lp.settled()) {
                break 'check;
            }
            let s = lp.status.clone();
            match s.state {
                SyncState::Idle if s.last_sync > 0 => {
                    match stage.take() {
                        Some(Stage::Waiting(cmd)) => {
                            let changes = cmd.changes();
                            let ui = matches!(cmd, VaultCommand::Ui(_));
                            match cmd {
                                VaultCommand::Ui(command) => lp.feed(Event::Command {
                                    req: COMMAND_REQ,
                                    command,
                                }),
                                VaultCommand::Resolve { id, choice } => {
                                    lp.feed(Event::Resolve { id, choice })
                                }
                            }
                            stage = Some(Stage::Sent { changes, ui });
                            continue 'check;
                        }
                        Some(Stage::Sent { changes, ui }) => {
                            result = if ui {
                                lp.ui.remove(&COMMAND_REQ)
                            } else {
                                Some(UiResult::Ok)
                            };
                            let failed = matches!(result, Some(UiResult::Error { .. }) | None);
                            if changes && !failed {
                                tracing::info!("синхронизация…");
                                lp.feed(Event::SyncNow);
                                stage = Some(Stage::Syncing);
                                continue 'check;
                            }
                        }
                        Some(Stage::Syncing) | None => {}
                    }
                    if s.read_only && s.pending > 0 {
                        eprintln!(
                            "предупреждение: устройство только для чтения — локальных правок, \
                         которые не уйдут на сервер: {}",
                            s.pending
                        );
                    }
                    return Ok(end(&mut lp, Outcome::Synced, result));
                }
                SyncState::Offline | SyncState::Error => {
                    return Ok(end(&mut lp, Outcome::Failed(reason_text(&s)), result));
                }
                SyncState::Blocked => {
                    let r = s.reason.clone().unwrap_or_default();
                    return Ok(end(&mut lp, Outcome::Blocked(r), result));
                }
                SyncState::NeedPassword if password_sent => {
                    return Ok(end(&mut lp, Outcome::NeedPassword, result));
                }
                _ => {}
            }
            break 'check;
        }
    }
}

/// Команда серверу без синка (подключение по коду и т.п.).
pub async fn command(
    dir: &Path,
    server: &str,
    token: Option<String>,
    device: &str,
    cmd: UiCommand,
) -> anyhow::Result<UiResult> {
    let exec = Arc::new(Exec::new(dir, server, token)?);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = EngineConfig {
        device_name: device.to_owned(),
        client: crate::config::CLIENT.to_owned(),
        ..EngineConfig::default()
    };
    let mut lp = Loop::new(cfg, None, exec, tx);
    lp.feed(Event::Command {
        req: 1,
        command: cmd,
    });
    loop {
        if let Some(r) = lp.ui.remove(&1) {
            return Ok(r);
        }
        match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
            Ok(Some(ev)) => lp.feed(ev),
            _ => anyhow::bail!("сервер не ответил"),
        }
    }
}
