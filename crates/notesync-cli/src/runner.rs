//! Цикл клиента: события (ответы исполнителя, таймер, слежение за папкой,
//! Ctrl-C) → ядро → действия → исполнитель. Ядро не `Send` и живёт в этой задаче,
//! ввод-вывод выполняется отдельными задачами tokio.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notesync_core::engine::{
    Action, Engine, EngineConfig, Event, LogLevel, Notice, SyncState, SyncStatus, UiCommand,
    UiResult,
};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config::{Config, STATE_DIR, write_private};
use crate::exec::Exec;

pub struct Options {
    /// Один цикл синка и выход (cron, скрипты, тесты).
    pub once: bool,
    /// Следить за папкой (иначе — только опрос и полное сканирование).
    pub watch: bool,
    /// Пароль шифрования, если vault зашифрован.
    pub password: Option<String>,
    /// Запомнить ключ в `.notesync/key` (0600), чтобы перезапуск не просил пароль.
    pub remember_key: bool,
    /// Включить шифрование vault'а паролем `password` (после первой синхронизации).
    pub enable_encryption: bool,
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
                if status.state != self.status.state {
                    tracing::info!(
                        "состояние: {:?}{}",
                        status.state,
                        status
                            .reason
                            .as_deref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default()
                    );
                }
                self.status = status;
            }
            Action::Notify { notice } => notify_user(&notice),
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

fn notify_user(n: &Notice) {
    match n {
        Notice::Conflict { path, copy, .. } => {
            tracing::warn!("конфликт: {path} — локальная версия сохранена как {copy}")
        }
        Notice::LocalCopySaved { path, copy } => {
            tracing::warn!("{path} отличался от серверного: локальная версия сохранена как {copy}")
        }
        Notice::Error { message, .. } => tracing::error!("{message}"),
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

/// Синхронизация папки: до Ctrl-C или (с `once`) до конца одного цикла.
pub async fn run(dir: &Path, cfg: &Config, opts: Options) -> anyhow::Result<Outcome> {
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
    let _watcher = if opts.watch && !opts.once {
        Some(watch(dir, tx.clone())?)
    } else {
        None
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
        });
    }
    let mut password_sent = opts.enable_encryption;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => lp.feed(ev),
                None => return Ok(Outcome::Interrupted),
            },
            () = tokio::time::sleep(lp.sleep()) => {
                lp.wake = None;
                lp.feed(Event::Tick);
            }
            _ = tokio::signal::ctrl_c() => {
                // Дать дописать начатое: индекс сохраняется после каждого файла.
                return Ok(Outcome::Interrupted);
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
                None if opts.once => return Ok(Outcome::NeedPassword),
                None => {}
            }
        }
        if opts.once && lp.settled() {
            let s = &lp.status;
            match s.state {
                SyncState::Idle if s.last_sync > 0 => return Ok(Outcome::Synced),
                SyncState::Offline | SyncState::Error => {
                    return Ok(Outcome::Failed(s.reason.clone().unwrap_or_default()));
                }
                SyncState::Blocked => {
                    return Ok(Outcome::Blocked(s.reason.clone().unwrap_or_default()));
                }
                SyncState::NeedPassword if password_sent => return Ok(Outcome::NeedPassword),
                _ => {}
            }
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
