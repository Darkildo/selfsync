//! Консольный клиент notesync: синхронизирует обычную папку тем же ядром, что и
//! плагин (сервер, NAS, headless-машина). Состояние — в `.notesync/` внутри папки.

pub mod config;
pub mod exec;
pub mod runner;
