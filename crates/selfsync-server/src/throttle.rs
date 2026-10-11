//! Лимит неудачных попыток: коды подключения и токены.
//!
//! В CGI процесс живёт один запрос, поэтому счётчики лежат в `server.db` (таблица
//! `throttle`), одна реализация на все режимы. Пишется только неудача: успешный
//! путь стоит одного `SELECT` по первичному ключу.

use std::net::IpAddr;
use std::time::Duration;

use axum::extract::ConnectInfo;
use http::{Extensions, HeaderMap};
use rusqlite::{Connection, OptionalExtension, params};

use crate::config::{Config, RateConfig, RateRule};
use crate::db::now_ms;

/// Адрес клиента, известный транспорту (CGI: `REMOTE_ADDR`).
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub IpAddr);

/// Глобальный ключ redeem блокирует на минуту после каждой неудачи сверх порога.
const GLOBAL_BLOCK: Duration = Duration::from_secs(60);
/// Строки без движения дольше суток удаляет sweep.
pub const KEEP_MS: i64 = 24 * 60 * 60 * 1000;

/// Счётчик: ключ в таблице и правило для него.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub key: String,
    pub rule: RateRule,
}

/// Адрес клиента: `X-Forwarded-For` (крайний правый, его добавил наш прокси) только
/// при `trusted_proxy`, иначе адрес соединения. Unix-сокет без прокси — `None`.
pub fn client_ip(config: &Config, headers: &HeaderMap, ext: &Extensions) -> Option<IpAddr> {
    if config.trusted_proxy
        && let Some(ip) = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .next_back()
            .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return Some(ip);
    }
    if let Some(p) = ext.get::<PeerAddr>() {
        return Some(p.0);
    }
    ext.get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip())
}

/// Адрес в ключе. IPv6 — по /64: столько обычно выдают одному клиенту.
fn addr_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

/// Ключи для обмена кода подключения: адрес (если известен) и общий.
pub fn redeem_keys(rate: &RateConfig, ip: Option<IpAddr>) -> Vec<Key> {
    let mut out = Vec::new();
    if let Some(ip) = ip
        && rate.redeem.attempts > 0
    {
        out.push(Key {
            key: format!("redeem:{}", addr_key(ip)),
            rule: rate.redeem,
        });
    }
    if rate.redeem_global > 0 {
        out.push(Key {
            key: "redeem:*".to_owned(),
            rule: RateRule {
                attempts: rate.redeem_global,
                window: rate.redeem.window,
                block: GLOBAL_BLOCK,
            },
        });
    }
    out
}

/// Ключ для неверных токенов. Без адреса все клиенты делят `auth:*`.
pub fn auth_keys(rate: &RateConfig, ip: Option<IpAddr>) -> Vec<Key> {
    if rate.auth.attempts == 0 {
        return Vec::new();
    }
    let key = match ip {
        Some(ip) => format!("auth:{}", addr_key(ip)),
        None => "auth:*".to_owned(),
    };
    vec![Key {
        key,
        rule: rate.auth,
    }]
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Заблокирован ли хоть один ключ: сколько секунд ждать (не меньше 1).
pub fn check(c: &Connection, keys: &[Key], now: i64) -> rusqlite::Result<Option<u64>> {
    let mut until: Option<i64> = None;
    let mut st = c.prepare_cached("SELECT blocked_until FROM throttle WHERE key = ?1")?;
    for k in keys {
        let b: Option<Option<i64>> = st.query_row(params![k.key], |r| r.get(0)).optional()?;
        if let Some(Some(b)) = b
            && b > now
        {
            until = Some(until.map_or(b, |u| u.max(b)));
        }
    }
    Ok(until.map(|u| u64::try_from((u - now + 999) / 1000).unwrap_or(1).max(1)))
}

/// Учесть неудачу. Окно фиксированное: истекло — счёт с начала. Порог достигнут —
/// ключ блокируется (повторно при каждой следующей неудаче в том же окне).
pub fn fail(c: &Connection, keys: &[Key], now: i64) -> rusqlite::Result<()> {
    let mut up = c.prepare_cached(
        "INSERT INTO throttle (key, window_start, failures) VALUES (?1, ?2, 1)
         ON CONFLICT(key) DO UPDATE SET
           failures = CASE WHEN ?2 - window_start >= ?3 THEN 1 ELSE failures + 1 END,
           window_start = CASE WHEN ?2 - window_start >= ?3 THEN ?2 ELSE window_start END
         RETURNING failures",
    )?;
    for k in keys {
        let failures: i64 = up.query_row(params![k.key, now, ms(k.rule.window)], |r| r.get(0))?;
        if failures >= i64::from(k.rule.attempts) {
            c.execute(
                "UPDATE throttle SET blocked_until = ?2 WHERE key = ?1",
                params![k.key, now.saturating_add(ms(k.rule.block))],
            )?;
            tracing::warn!(key = %k.key, failures, "лимит неудачных попыток: блокировка");
        }
    }
    Ok(())
}

/// Удалить счётчики, которые не менялись сутки и не блокируют.
pub fn cleanup(c: &Connection, now: i64) -> rusqlite::Result<usize> {
    c.execute(
        "DELETE FROM throttle WHERE window_start < ?1
           AND (blocked_until IS NULL OR blocked_until < ?2)",
        params![now - KEEP_MS, now],
    )
}

/// Обёртка для обработчиков: проверка перед делом.
pub fn check_now(c: &Connection, keys: &[Key]) -> rusqlite::Result<Option<u64>> {
    if keys.is_empty() {
        return Ok(None);
    }
    check(c, keys, now_ms())
}

/// Обёртка для обработчиков: учесть неудачу сейчас.
pub fn fail_now(c: &Connection, keys: &[Key]) -> rusqlite::Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    fail(c, keys, now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::server::migrate_server;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate_server(&mut c).unwrap();
        c
    }

    fn rule(attempts: u32, window_s: u64, block_s: u64) -> RateRule {
        RateRule {
            attempts,
            window: Duration::from_secs(window_s),
            block: Duration::from_secs(block_s),
        }
    }

    #[test]
    fn blocks_after_threshold_and_expires() {
        let c = db();
        let k = vec![Key {
            key: "redeem:1.2.3.4".into(),
            rule: rule(3, 60, 30),
        }];
        let t = 1_000_000;
        for i in 0..2 {
            fail(&c, &k, t + i).unwrap();
            assert_eq!(check(&c, &k, t + i).unwrap(), None);
        }
        fail(&c, &k, t + 2).unwrap();
        assert_eq!(check(&c, &k, t + 2).unwrap(), Some(30));
        assert_eq!(check(&c, &k, t + 2 + 29_500).unwrap(), Some(1));
        assert_eq!(check(&c, &k, t + 2 + 30_000).unwrap(), None);
        // новое окно — счёт с начала
        fail(&c, &k, t + 61_000).unwrap();
        assert_eq!(check(&c, &k, t + 61_000).unwrap(), None);
    }

    #[test]
    fn keys_are_independent_and_max_wins() {
        let c = db();
        let a = Key {
            key: "a".into(),
            rule: rule(1, 60, 10),
        };
        let b = Key {
            key: "b".into(),
            rule: rule(1, 60, 50),
        };
        fail(&c, std::slice::from_ref(&a), 0).unwrap();
        assert_eq!(check(&c, std::slice::from_ref(&b), 0).unwrap(), None);
        fail(&c, std::slice::from_ref(&b), 0).unwrap();
        assert_eq!(check(&c, &[a, b], 0).unwrap(), Some(50));
    }

    #[test]
    fn keys_from_config() {
        let mut rate = RateConfig::default();
        let ip: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        let ks = redeem_keys(&rate, Some(ip));
        assert_eq!(ks[0].key, "redeem:2001:db8:1:2::/64");
        assert_eq!(ks[1].key, "redeem:*");
        assert_eq!(ks[1].rule.attempts, 200);
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert_eq!(auth_keys(&rate, Some(mapped))[0].key, "auth:10.0.0.1");
        assert_eq!(auth_keys(&rate, None)[0].key, "auth:*");
        assert_eq!(redeem_keys(&rate, None).len(), 1);
        rate.redeem.attempts = 0;
        rate.redeem_global = 0;
        rate.auth.attempts = 0;
        assert!(redeem_keys(&rate, Some(ip)).is_empty());
        assert!(auth_keys(&rate, Some(ip)).is_empty());
    }

    #[test]
    fn cleanup_keeps_fresh_and_blocked() {
        let c = db();
        let now = 10 * KEEP_MS;
        let k = |name: &str, attempts| Key {
            key: name.into(),
            rule: rule(attempts, 60, 10 * 24 * 3600),
        };
        fail(&c, &[k("old", 100)], now - KEEP_MS - 1).unwrap();
        fail(&c, &[k("blocked", 1)], now - KEEP_MS - 1).unwrap();
        fail(&c, &[k("fresh", 100)], now).unwrap();
        assert_eq!(cleanup(&c, now).unwrap(), 1);
        let left: Vec<String> = c
            .prepare("SELECT key FROM throttle ORDER BY key")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, ["blocked", "fresh"]);
    }
}
