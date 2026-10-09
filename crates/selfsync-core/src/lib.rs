//! Sans-IO ядро синхронизации selfsync.
//!
//! Ядро не делает ввода-вывода: протокол, локальный индекс, планировщик синка,
//! 3-way merge и шифрование существуют здесь в одном экземпляре и используются
//! плагином (через WASM), CLI-клиентом и симуляцией. Сервер берёт отсюда валидацию
//! путей и формат блоба (для `import`).

pub mod blob;
pub mod crypto;
pub mod engine;
pub mod exclude;
pub mod hash;
pub mod index;
pub mod merge;
pub mod path;

pub use selfsync_proto as proto;
pub use selfsync_proto::v1 as pb;
