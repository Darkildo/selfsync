//! Хук после синка (`run --exec`): команда оболочки после каждого цикла, который
//! что-то изменил. Хуки не накладываются: пока один выполняется, следующие циклы
//! копятся и потом дают один запуск с объединённым списком путей. Ошибка или
//! таймаут хука синк не останавливают.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::STATE_DIR;

/// Таймаут хука по умолчанию.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Файл со списком путей последнего запуска (по одному на строку).
pub const CHANGES_FILE: &str = "last-changes.txt";

#[derive(Debug, Clone)]
pub struct HookConfig {
    pub command: String,
    pub timeout: Duration,
}

/// Что изменили циклы с прошлого запуска хука.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Batch {
    pub pulled: u64,
    pub pushed: u64,
    pub paths: BTreeSet<String>,
}

impl Batch {
    fn merge(&mut self, other: Batch) {
        self.pulled += other.pulled;
        self.pushed += other.pushed;
        self.paths.extend(other.paths);
    }
}

/// Очередь хука: `send` из цикла клиента, запуск — в своей задаче.
pub struct Hook {
    tx: mpsc::UnboundedSender<Batch>,
    task: JoinHandle<()>,
}

impl Hook {
    pub fn start(dir: &Path, cfg: HookConfig) -> Hook {
        let (tx, rx) = mpsc::unbounded_channel();
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        let task = tokio::spawn(worker(dir, cfg, rx));
        Hook { tx, task }
    }

    pub fn send(&self, batch: Batch) {
        let _ = self.tx.send(batch);
    }

    /// Дождаться, пока хук отработает всё накопленное (выход `run --once`).
    pub async fn finish(self) {
        drop(self.tx);
        let _ = self.task.await;
    }
}

async fn worker(dir: PathBuf, cfg: HookConfig, mut rx: mpsc::UnboundedReceiver<Batch>) {
    while let Some(mut batch) = rx.recv().await {
        // Всё, что пришло, пока хук работал, — одним запуском.
        while let Ok(more) = rx.try_recv() {
            batch.merge(more);
        }
        run_once(&dir, &cfg, &batch).await;
    }
}

/// Один запуск: список путей в файл, `sh -c` в папке с переменными окружения.
pub async fn run_once(dir: &Path, cfg: &HookConfig, batch: &Batch) {
    let list = dir.join(STATE_DIR).join(CHANGES_FILE);
    let mut text = String::new();
    for p in &batch.paths {
        text.push_str(p);
        text.push('\n');
    }
    if let Err(e) = std::fs::write(&list, text) {
        tracing::warn!("хук: список изменений не записан ({}): {e}", list.display());
    }
    let mut cmd = shell(&cfg.command);
    cmd.current_dir(dir)
        .env("SELFSYNC_DIR", dir)
        .env("SELFSYNC_PULLED", batch.pulled.to_string())
        .env("SELFSYNC_PUSHED", batch.pushed.to_string())
        .env("SELFSYNC_CHANGES_FILE", &list)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("хук не запустился: {e}");
            return;
        }
    };
    tracing::info!(
        "хук: {} (пришло {}, отправлено {}, путей {})",
        cfg.command,
        batch.pulled,
        batch.pushed,
        batch.paths.len()
    );
    match tokio::time::timeout(cfg.timeout, child.wait()).await {
        Ok(Ok(st)) if st.success() => tracing::info!("хук выполнен"),
        Ok(Ok(st)) => tracing::warn!("хук завершился с ошибкой: {st}"),
        Ok(Err(e)) => tracing::warn!("хук: {e}"),
        Err(_) => {
            // Вся группа: `sh -c` мог запустить дочерние процессы.
            #[cfg(unix)]
            if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
                // SAFETY: kill(2) с отрицательным pid — сигнал группе процессов.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            tracing::warn!(
                "хук не уложился в {} и остановлен",
                humantime::format_duration(cfg.timeout)
            );
        }
    }
}

fn shell(command: &str) -> tokio::process::Command {
    if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}
