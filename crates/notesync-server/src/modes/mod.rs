//! Режимы запуска. Вся логика и роутинг общие, отличается только транспорт.

pub mod cgi;

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::net::{TcpListener, UnixListener};

use crate::api::router;
use crate::db::now_ms;
use crate::state::AppState;

pub enum AnyListener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl AnyListener {
    pub fn describe(&self) -> String {
        match self {
            AnyListener::Tcp(l) => l
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "tcp".into()),
            AnyListener::Unix(l) => l
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| format!("unix:{}", p.display())))
                .unwrap_or_else(|| "unix".into()),
        }
    }
}

/// `--listen`: `127.0.0.1:8085` или `unix:/run/notesync.sock`.
pub async fn bind(spec: &str) -> anyhow::Result<AnyListener> {
    if let Some(path) = spec.strip_prefix("unix:") {
        let _ = std::fs::remove_file(path);
        let l = UnixListener::bind(path).with_context(|| format!("не удалось слушать {spec}"))?;
        Ok(AnyListener::Unix(l))
    } else {
        let l = TcpListener::bind(spec)
            .await
            .with_context(|| format!("не удалось слушать {spec}"))?;
        Ok(AnyListener::Tcp(l))
    }
}

/// Сокет, переданный systemd (`LISTEN_FDS`/`LISTEN_PID`), TCP или Unix.
pub fn from_systemd() -> anyhow::Result<AnyListener> {
    let mut fds = listenfd::ListenFd::from_env();
    if fds.len() == 0 {
        bail!(
            "systemd не передал сокет. Запускайте через systemd socket unit \
             (deploy/systemd/notesync.socket) или для проверки: \
             systemd-socket-activate -l 8085 notesync socket"
        );
    }
    if let Ok(Some(l)) = fds.take_tcp_listener(0) {
        l.set_nonblocking(true)?;
        return Ok(AnyListener::Tcp(TcpListener::from_std(l)?));
    }
    if let Ok(Some(l)) = fds.take_unix_listener(0) {
        l.set_nonblocking(true)?;
        return Ok(AnyListener::Unix(UnixListener::from_std(l)?));
    }
    bail!("переданный systemd дескриптор — не слушающий TCP- или Unix-сокет")
}

/// Ждёт простоя: нет запросов в обработке и нет активности дольше `idle`.
/// Ожидающие `/v1/wait` в обработке не считаются (см. `api::access`).
pub async fn idle_watch(state: AppState, idle: Duration) {
    if idle.is_zero() {
        std::future::pending::<()>().await;
    }
    let idle_ms = i64::try_from(idle.as_millis()).unwrap_or(i64::MAX);
    let tick = (idle / 4).clamp(Duration::from_millis(50), Duration::from_secs(1));
    loop {
        tokio::time::sleep(tick).await;
        let busy = state.activity.in_flight.load(Ordering::SeqCst) > 0;
        let last = state.activity.last.load(Ordering::Relaxed);
        if !busy && now_ms() - last >= idle_ms {
            tracing::info!(idle_ms, "простой: завершаюсь");
            return;
        }
    }
}

async fn terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM"),
            _ = int.recv() => tracing::info!("SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Обслуживает соединения до сигнала или простоя, затем graceful shutdown и WAL
/// checkpoint.
pub async fn run(state: AppState, listener: AnyListener, idle: Duration) -> anyhow::Result<()> {
    let app = router(state.clone());
    let st = state.clone();
    let shutdown = async move {
        tokio::select! {
            () = terminate_signal() => {}
            () = idle_watch(st.clone(), idle) => {}
        }
        // Разбудить ожидающие long-poll, чтобы graceful shutdown не ждал их таймаута.
        st.shutdown.send_replace(true);
    };
    tracing::info!(listen = %listener.describe(), idle_s = idle.as_secs(), "notesync слушает");
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    match listener {
        AnyListener::Tcp(l) => axum::serve(l, app).with_graceful_shutdown(shutdown).await?,
        AnyListener::Unix(l) => axum::serve(l, app).with_graceful_shutdown(shutdown).await?,
    }
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
    let st = state.clone();
    tokio::task::spawn_blocking(move || st.checkpoint()).await?;
    tracing::info!("остановлен");
    Ok(())
}
