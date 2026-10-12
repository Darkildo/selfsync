//! Проверка соединения (F15): по шагу на типичную ошибку настройки — адрес и TLS,
//! прокси, версия, часы, токен, лимит тела запроса, long-poll. Шаг возвращает код
//! итога и подробность; тексты для людей — в плагине и CLI.

use prost::Message;
use selfsync_proto::{HEADER_PROTO, PROTO_MINOR, PROTO_VERSION, parse_proto};

use super::api;
use super::ctx::{Ctx, SyncError};
use super::types::{DiagCheck, DiagLevel, HttpRequest};
use crate::hash::Hash;

/// Тело для проверки лимита: больше `client_max_body_size 1m` nginx по умолчанию,
/// меньше однократной загрузки сервера (8 МиБ).
const BODY_PROBE: usize = 1_500_000;
/// Сколько long-poll должен держать запрос, секунд.
const WAIT_PROBE_S: u32 = 3;
/// Расхождение часов, после которого предупредить.
const SKEW_WARN_MS: i64 = 5 * 60 * 1000;

fn check(step: &str, level: DiagLevel, code: &str, detail: impl Into<String>) -> DiagCheck {
    DiagCheck {
        step: step.to_owned(),
        level,
        code: code.to_owned(),
        detail: detail.into(),
    }
}

/// Причина сетевой ошибки по тексту исполнителя (Node, Electron, Capacitor).
pub fn classify_network(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    let any = |xs: &[&str]| xs.iter().any(|x| m.contains(x));
    if any(&[
        "cert",
        "ssl",
        "tls",
        "self signed",
        "self-signed",
        "handshake",
    ]) {
        "tls"
    } else if any(&[
        "enotfound",
        "name_not_resolved",
        "getaddrinfo",
        "dns",
        "unknown host",
    ]) {
        "dns"
    } else if any(&["econnrefused", "connection_refused", "refused"]) {
        "refused"
    } else if any(&["timeout", "timed out", "etimedout", "timed_out"]) {
        "timeout"
    } else {
        "network"
    }
}

/// Значение поля плоского JSON `/v1/health` (строка или число) без разбора JSON целиком.
fn json_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let rest = &body[body.find(&pat)? + pat.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        return s.find('"').map(|end| &s[..end]);
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// HTTP-дата (`Sun, 06 Nov 1994 08:49:37 GMT`) → мс Unix.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let mut it = s.split_whitespace().skip(1);
    let day: i64 = it.next()?.parse().ok()?;
    let month = match it.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi, sec) = (hms.next()??, hms.next()??, hms.next()??);
    // Дни от эпохи по гражданскому календарю (алгоритм Хиннанта).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Все шаги по порядку; после отказа шага, без которого дальше проверять нечего,
/// остальные не выполняются.
pub(crate) async fn diagnose(cx: &Ctx) -> Vec<DiagCheck> {
    let mut out = Vec::new();
    if !server_steps(cx, &mut out).await {
        return out;
    }
    let Some((seq, read_only)) = token_step(cx, &mut out).await else {
        return out;
    };
    body_step(cx, read_only, &mut out).await;
    wait_step(cx, seq, &mut out).await;
    out
}

/// Адрес, прокси, версия, часы. `false` — сервер недоступен или по адресу не selfsync.
async fn server_steps(cx: &Ctx, out: &mut Vec<DiagCheck>) -> bool {
    let sent = cx.now();
    let r = HttpRequest {
        method: "GET".into(),
        path: "/v1/health".into(),
        headers: vec![(HEADER_PROTO.to_owned(), selfsync_proto::proto_header())],
        body: vec![],
        auth: false,
        timeout_ms: 15_000,
    };
    let (status, headers, body) = match cx.http(r).await {
        Ok(x) => x,
        Err(SyncError::Network(m)) => {
            out.push(check("server", DiagLevel::Fail, classify_network(&m), m));
            return false;
        }
        Err(e) => {
            out.push(check("server", DiagLevel::Fail, "network", e.to_string()));
            return false;
        }
    };
    let text = String::from_utf8_lossy(&body);
    if status != 200 || json_field(&text, "status") != Some("ok") {
        out.push(check(
            "server",
            DiagLevel::Fail,
            "not_selfsync",
            format!("HTTP {status}"),
        ));
        return false;
    }
    let version = json_field(&text, "version").unwrap_or("?").to_owned();
    out.push(check("server", DiagLevel::Ok, "ok", version.clone()));
    let major: u32 = json_field(&text, "proto")
        .and_then(|x| x.parse().ok())
        .unwrap_or(PROTO_VERSION);
    let minor: u32 = json_field(&text, "proto_minor")
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    // Сервер 0.2.1+ шлёт заголовок в каждом ответе: нет его — режет прокси.
    let proxy = if minor >= 1
        && header(&headers, HEADER_PROTO)
            .and_then(parse_proto)
            .is_none()
    {
        check("proxy", DiagLevel::Warn, "headers_stripped", HEADER_PROTO)
    } else {
        check("proxy", DiagLevel::Ok, "ok", "")
    };
    out.push(proxy);
    out.push(if major != PROTO_VERSION {
        check(
            "version",
            DiagLevel::Fail,
            "proto_major",
            format!("{major}.{minor}"),
        )
    } else if minor < PROTO_MINOR {
        check("version", DiagLevel::Warn, "server_old", version)
    } else {
        check("version", DiagLevel::Ok, "ok", version)
    });
    let got = cx.now();
    out.push(match header(&headers, "date").and_then(parse_http_date) {
        // Дата сервера — с точностью до секунды, где-то между отправкой и ответом.
        Some(server) => {
            let skew = server - (sent + got) / 2;
            if skew.abs() > SKEW_WARN_MS {
                check("clock", DiagLevel::Warn, "skew", format!("{}", skew / 1000))
            } else {
                check("clock", DiagLevel::Ok, "ok", "")
            }
        }
        None => check("clock", DiagLevel::Skip, "no_date", ""),
    });
    true
}

/// Токен: `(seq, только для чтения)` или `None`, если дальше проверять нечего.
async fn token_step(cx: &Ctx, out: &mut Vec<DiagCheck>) -> Option<(u64, bool)> {
    match api::changes(cx, 0, 1).await {
        Ok(r) => {
            let vs = r.vault.unwrap_or_default();
            out.push(check(
                "token",
                DiagLevel::Ok,
                if vs.read_only { "read_only" } else { "ok" },
                "",
            ));
            Some((vs.seq, vs.read_only))
        }
        Err(e) => {
            let code = match &e {
                SyncError::Unauthorized => "unauthorized",
                SyncError::ProtoUnsupported(_) => "client_old",
                SyncError::RateLimited { .. } => "rate_limited",
                SyncError::Network(m) => classify_network(m),
                _ => "error",
            };
            out.push(check("token", DiagLevel::Fail, code, e.to_string()));
            None
        }
    }
}

/// Лимит тела у прокси: блоб с заведомо неверным хэшем сервер прочитает целиком и
/// откажет `hash_mismatch`, а прокси с лимитом ответит 413 раньше.
async fn body_step(cx: &Ctx, read_only: bool, out: &mut Vec<DiagCheck>) {
    if read_only {
        out.push(check("body", DiagLevel::Skip, "read_only", ""));
        return;
    }
    let wrong = Hash::of(b"selfsync diagnose: wrong hash");
    let r = HttpRequest {
        method: "PUT".into(),
        path: format!("/v1/blobs/{}", wrong.to_hex()),
        headers: vec![
            (HEADER_PROTO.to_owned(), selfsync_proto::proto_header()),
            (
                "content-type".to_owned(),
                selfsync_proto::CONTENT_TYPE_OCTETS.to_owned(),
            ),
        ],
        body: vec![0x5a; BODY_PROBE],
        auth: true,
        timeout_ms: 60_000,
    };
    out.push(match cx.http(r).await {
        Ok((400, ..)) => check("body", DiagLevel::Ok, "ok", ""),
        // 413 от самого selfsync — его лимит файла (protobuf `too_large`), от прокси —
        // страница прокси.
        Ok((413, _, body))
            if selfsync_proto::v1::Error::decode(&body[..])
                .is_ok_and(|e| e.code == "too_large") =>
        {
            check("body", DiagLevel::Ok, "server_limit", "")
        }
        Ok((413, ..)) => check("body", DiagLevel::Fail, "body_limit", "HTTP 413"),
        Ok((507, ..)) => check("body", DiagLevel::Skip, "quota", ""),
        Ok((status, ..)) => check(
            "body",
            DiagLevel::Warn,
            "unexpected",
            format!("HTTP {status}"),
        ),
        // Прокси, обрывающий большое тело, выглядит как сетевая ошибка.
        Err(e) => check("body", DiagLevel::Fail, "body_limit", e.to_string()),
    });
}

/// Long-poll держится: иначе (CGI, прокси без long-poll) лучше опрос.
async fn wait_step(cx: &Ctx, seq: u64, out: &mut Vec<DiagCheck>) {
    if !cx.with(|s| s.cfg.use_wait) {
        out.push(check("longpoll", DiagLevel::Skip, "off", ""));
        return;
    }
    let t0 = cx.now();
    out.push(match api::wait(cx, seq, WAIT_PROBE_S).await {
        Ok(vs) if vs.seq > seq => check("longpoll", DiagLevel::Skip, "changed", ""),
        Ok(_) if cx.now() - t0 < i64::from(WAIT_PROBE_S) * 1000 - 1000 => {
            check("longpoll", DiagLevel::Warn, "no_longpoll", "")
        }
        Ok(_) => check("longpoll", DiagLevel::Ok, "ok", ""),
        Err(e) => check("longpoll", DiagLevel::Warn, "failed", e.to_string()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Mon, 12 Oct 2026 11:47:28 GMT"),
            Some(1_791_805_648_000)
        );
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn health_fields() {
        let b = r#"{"proto":1,"proto_minor":3,"status":"ok","version":"0.4.0"}"#;
        assert_eq!(json_field(b, "status"), Some("ok"));
        assert_eq!(json_field(b, "version"), Some("0.4.0"));
        assert_eq!(json_field(b, "proto_minor"), Some("3"));
        assert_eq!(json_field(b, "missing"), None);
    }

    #[test]
    fn network_reasons() {
        assert_eq!(classify_network("net::ERR_CERT_AUTHORITY_INVALID"), "tls");
        assert_eq!(classify_network("self signed certificate"), "tls");
        assert_eq!(
            classify_network("getaddrinfo ENOTFOUND sync.example"),
            "dns"
        );
        assert_eq!(
            classify_network("connect ECONNREFUSED 1.2.3.4:443"),
            "refused"
        );
        assert_eq!(classify_network("тайм-аут: request timed out"), "timeout");
        assert_eq!(classify_network("socket hang up"), "network");
    }
}
