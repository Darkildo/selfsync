//! Типы протокола selfsync, сгенерированные из `proto/selfsync/v1/sync.proto`.

/// Мажорная версия протокола; передаётся в заголовке [`HEADER_PROTO`].
pub const PROTO_VERSION: u32 = 1;

/// Заголовок версии протокола в каждом запросе.
pub const HEADER_PROTO: &str = "x-selfsync-proto";

/// `Content-Type` метаданных.
pub const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";

/// `Content-Type` содержимого блобов.
pub const CONTENT_TYPE_OCTETS: &str = "application/octet-stream";

#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/selfsync.v1.rs"));
}

pub use prost::Message;
