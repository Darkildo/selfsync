//! Типы протокола selfsync, сгенерированные из `proto/selfsync/v1/sync.proto`.

/// Мажорная версия протокола; передаётся в заголовке [`HEADER_PROTO`]. Другая
/// мажорная — несовместимость целиком (сервер отвечает 426).
pub const PROTO_VERSION: u32 = 1;

/// Минорная версия: растёт с каждой обратно совместимой возможностью, которую
/// нужно уметь обеим сторонам. Клиент и сервер 0.1.x — `1.0` (минорной не слали).
///
/// | минорная | релиз | возможность |
/// |---|---|---|
/// | 1 | 0.2.1 | устройства только для чтения ([`MINOR_READ_ONLY`]) |
/// | 2 | 0.3.0 | смена ключа шифрования: поколения ключа ([`MINOR_REKEY`]) |
/// | 3 | 0.4.0 | квоты, стирание истории, состояние хранилища ([`MINOR_QUOTA`]) |
pub const PROTO_MINOR: u32 = 3;

/// Устройства только для чтения: сервер, который их не знает, молча выдал бы по
/// ссылке «только чтение» полный доступ, а клиент, который их не знает, пытался бы
/// отправлять правки.
pub const MINOR_READ_ONLY: u32 = 1;

/// Смена ключа шифрования: пути с поколением ключа (`Path.key_epoch`), purge по
/// поколению. Сервер, который их не знает, смешал бы поколения; клиент, который их
/// не знает, не открыл бы vault после смены.
pub const MINOR_REKEY: u32 = 2;

/// Квоты (`VaultState.used_bytes`…, отказ `507 quota_exceeded`), стирание старых
/// версий (`POST /v1/history/purge`), состояние хранилища и размер файла в истории.
/// Клиент предлагает стереть историю только серверу, который это умеет.
pub const MINOR_QUOTA: u32 = 3;

/// Заголовок версии протокола: в каждом запросе клиента и (с `1.1`) в каждом ответе
/// сервера. Значение — `мажорная.минорная` или только `мажорная` (минорная 0).
pub const HEADER_PROTO: &str = "x-selfsync-proto";

/// Заголовок клиента в запросах: `<вид>/<версия>` (`plugin/0.3.0`, `cli/0.3.0`).
/// Сервер запоминает его у устройства — показать админу и другим устройствам, кто
/// чем подключён. Не влияет на поведение сервера (для этого — [`HEADER_PROTO`]).
pub const HEADER_CLIENT: &str = "x-selfsync-client";

/// Значение [`HEADER_PROTO`] этой сборки.
pub fn proto_header() -> String {
    format!("{PROTO_VERSION}.{PROTO_MINOR}")
}

/// `"1"` → `(1, 0)`, `"1.1"` → `(1, 1)`. Мажорная не разбирается — `None`;
/// неразборчивая минорная считается нулевой (умеет только базовый протокол).
pub fn parse_proto(s: &str) -> Option<(u32, u32)> {
    let mut it = s.trim().splitn(2, '.');
    let major = it.next()?.trim().parse().ok()?;
    let minor = it.next().and_then(|m| m.trim().parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// `Content-Type` метаданных.
pub const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";

/// `Content-Type` содержимого блобов.
pub const CONTENT_TYPE_OCTETS: &str = "application/octet-stream";

#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/selfsync.v1.rs"));
}

pub use prost::Message;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_header_round_trips() {
        assert_eq!(
            parse_proto(&proto_header()),
            Some((PROTO_VERSION, PROTO_MINOR))
        );
        assert_eq!(parse_proto("1"), Some((1, 0)));
        assert_eq!(parse_proto(" 1.1 "), Some((1, 1)));
        assert_eq!(parse_proto("2.0"), Some((2, 0)));
        assert_eq!(parse_proto("1.x"), Some((1, 0)));
        assert_eq!(parse_proto("1.2.3"), Some((1, 0)));
        assert_eq!(parse_proto("v1"), None);
        assert_eq!(parse_proto(""), None);
    }
}
