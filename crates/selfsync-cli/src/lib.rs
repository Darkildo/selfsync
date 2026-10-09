//! Консольный клиент selfsync: синхронизирует обычную папку тем же ядром, что и
//! плагин (сервер, NAS, headless-машина). Состояние — в `.selfsync/` внутри папки.

pub mod config;
pub mod exec;
pub mod runner;
