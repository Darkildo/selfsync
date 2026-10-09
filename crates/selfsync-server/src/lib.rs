//! Сервер selfsync: упорядоченный лог изменений и хранилище байтов.
//!
//! Один бинарь `selfsync`, режим выбирается подкомандой (`cgi`, `socket`, `serve`);
//! роутер и логика общие. Между запросами состояние в памяти не хранится.

pub mod api;
pub mod blobs;
pub mod cmd;
pub mod config;
pub mod db;
pub mod error;
pub mod gc;
pub mod modes;
pub mod state;
pub mod sweep;

pub use api::router;
pub use config::Config;
pub use state::{AppState, Mode};

/// Логи — только в stderr (в CGI stdout занят ответом).
pub fn init_logging(filter: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init();
}
