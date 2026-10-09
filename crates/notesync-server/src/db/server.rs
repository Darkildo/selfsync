//! `server.db`: устройства (токены) и одноразовые коды подключения, общие для всех
//! vault'ов. Токены хранятся только как sha256, показываются один раз.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{migrate, now_ms};

pub const TOKEN_PREFIX: &str = "ns_";
/// Время жизни кода подключения.
pub const JOIN_TTL_MS: i64 = 15 * 60 * 1000;
/// `last_seen` обновляется не чаще раза в минуту.
const LAST_SEEN_STEP_MS: i64 = 60 * 1000;

const MIGRATIONS: &[&str] = &["
CREATE TABLE devices (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  vault       TEXT NOT NULL,
  name        TEXT NOT NULL,
  token_hash  BLOB NOT NULL UNIQUE,
  created_at  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL DEFAULT 0,
  revoked_at  INTEGER
);
CREATE INDEX devices_vault ON devices(vault);
CREATE TABLE join_codes (
  code_hash   BLOB PRIMARY KEY,
  vault       TEXT NOT NULL,
  name        TEXT NOT NULL,
  created_by  INTEGER,
  created_at  INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  used_at     INTEGER,
  used_by     INTEGER
);
"];

pub fn migrate_server(c: &mut Connection) -> rusqlite::Result<()> {
    migrate(c, MIGRATIONS)
}

/// Аутентифицированное устройство.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    pub id: u32,
    pub vault: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: u32,
    pub vault: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub revoked: bool,
}

/// Новый токен: 32 случайных байта в base64url с префиксом `ns_`.
pub fn generate_token() -> anyhow::Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(b)))
}

pub fn hash_secret(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

/// Код подключения: 80 бит в base32 без похожих символов (16 знаков).
pub fn generate_join_code() -> anyhow::Result<String> {
    const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut b = [0u8; 10];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let mut bits: u128 = 0;
    for x in b {
        bits = (bits << 8) | u128::from(x);
    }
    let mut out = String::with_capacity(16);
    for i in (0..16).rev() {
        let idx = usize::try_from((bits >> (i * 5)) & 31).unwrap_or(0);
        out.push(char::from(ALPHABET[idx]));
    }
    Ok(out)
}

/// Проверка имени vault'а: `[a-z0-9-]{1,64}`.
pub fn valid_vault_name(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Создаёт устройство и возвращает (id, токен).
pub fn add_device(c: &Connection, vault: &str, name: &str) -> anyhow::Result<(u32, String)> {
    anyhow::ensure!(
        valid_vault_name(vault),
        "имя vault'а должно быть [a-z0-9-]{{1,64}}"
    );
    let name = name.trim();
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 128,
        "имя устройства: 1–128 байт"
    );
    let token = generate_token()?;
    c.execute(
        "INSERT INTO devices (vault, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![vault, name, hash_secret(&token), now_ms()],
    )?;
    let id = u32::try_from(c.last_insert_rowid())?;
    Ok((id, token))
}

/// Находит устройство по токену. Неизвестный или отозванный — `None`.
pub fn authenticate(c: &Connection, token: &str) -> rusqlite::Result<Option<DeviceAuth>> {
    if !token.starts_with(TOKEN_PREFIX) || token.len() > 128 {
        return Ok(None);
    }
    let h = hash_secret(token);
    let row: Option<(i64, String, String, Vec<u8>, i64)> = c
        .query_row(
            "SELECT id, vault, name, token_hash, last_seen FROM devices
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![h],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, vault, name, stored, last_seen)) = row else {
        return Ok(None);
    };
    // Поиск по хэшу сам по себе не раскрывает токен, но сравнение — константное.
    if !bool::from(stored.ct_eq(&h)) {
        return Ok(None);
    }
    let now = now_ms();
    if now - last_seen > LAST_SEEN_STEP_MS {
        c.execute(
            "UPDATE devices SET last_seen = ?1 WHERE id = ?2",
            params![now, id],
        )?;
    }
    Ok(Some(DeviceAuth {
        id: u32::try_from(id).unwrap_or(0),
        vault,
        name,
    }))
}

fn row_to_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    let id: i64 = r.get(0)?;
    let revoked: Option<i64> = r.get(5)?;
    Ok(DeviceRow {
        id: u32::try_from(id).unwrap_or(0),
        vault: r.get(1)?,
        name: r.get(2)?,
        created_at: r.get(3)?,
        last_seen: r.get(4)?,
        revoked: revoked.is_some(),
    })
}

pub fn list_devices(c: &Connection, vault: Option<&str>) -> rusqlite::Result<Vec<DeviceRow>> {
    let mut out = Vec::new();
    match vault {
        Some(v) => {
            let mut st = c.prepare(
                "SELECT id, vault, name, created_at, last_seen, revoked_at FROM devices
                 WHERE vault = ?1 ORDER BY id",
            )?;
            for d in st.query_map(params![v], row_to_device)? {
                out.push(d?);
            }
        }
        None => {
            let mut st = c.prepare(
                "SELECT id, vault, name, created_at, last_seen, revoked_at FROM devices
                 ORDER BY vault, id",
            )?;
            for d in st.query_map([], row_to_device)? {
                out.push(d?);
            }
        }
    }
    Ok(out)
}

/// Все vault'ы, у которых есть устройства.
pub fn vault_names(c: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut st = c.prepare("SELECT DISTINCT vault FROM devices ORDER BY vault")?;
    let rows = st.query_map([], |r| r.get(0))?;
    rows.collect()
}

pub fn revoke_by_id(c: &Connection, vault: &str, id: u32) -> rusqlite::Result<bool> {
    let n = c.execute(
        "UPDATE devices SET revoked_at = ?1 WHERE id = ?2 AND vault = ?3 AND revoked_at IS NULL",
        params![now_ms(), id, vault],
    )?;
    Ok(n > 0)
}

pub fn revoke_by_name(c: &Connection, vault: Option<&str>, name: &str) -> rusqlite::Result<usize> {
    match vault {
        Some(v) => c.execute(
            "UPDATE devices SET revoked_at = ?1 WHERE name = ?2 AND vault = ?3 AND revoked_at IS NULL",
            params![now_ms(), name, v],
        ),
        None => c.execute(
            "UPDATE devices SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
            params![now_ms(), name],
        ),
    }
}

pub fn count_devices(c: &Connection, vault: &str) -> rusqlite::Result<u32> {
    c.query_row(
        "SELECT COUNT(*) FROM devices WHERE vault = ?1 AND revoked_at IS NULL",
        params![vault],
        |r| r.get(0),
    )
}

/// Создаёт одноразовый код подключения. Возвращает (код, истекает_в).
pub fn create_join_code(
    c: &Connection,
    vault: &str,
    name: &str,
    created_by: Option<u32>,
) -> anyhow::Result<(String, i64)> {
    anyhow::ensure!(valid_vault_name(vault), "неверное имя vault'а");
    let code = generate_join_code()?;
    let now = now_ms();
    let expires = now + JOIN_TTL_MS;
    // Старые коды убираем заодно: таблица не растёт.
    c.execute(
        "DELETE FROM join_codes WHERE expires_at < ?1",
        params![now - JOIN_TTL_MS],
    )?;
    c.execute(
        "INSERT INTO join_codes (code_hash, vault, name, created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            hash_secret(&code),
            vault,
            name.trim(),
            created_by,
            now,
            expires
        ],
    )?;
    Ok((code, expires))
}

/// Состояние кода без его использования (для HTML-страницы).
pub fn peek_join_code(c: &Connection, code: &str) -> rusqlite::Result<Option<(String, String)>> {
    c.query_row(
        "SELECT vault, name FROM join_codes WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
        params![hash_secret(code), now_ms()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// Обменивает код на новое устройство. Код одноразовый: повтор и истёкший — `None`.
pub fn redeem_join_code(
    c: &mut Connection,
    code: &str,
    name: &str,
) -> anyhow::Result<Option<(u32, String, String, String)>> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT vault, name FROM join_codes
             WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
            params![hash_secret(code), now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((vault, default_name)) = row else {
        return Ok(None);
    };
    let name = if name.trim().is_empty() {
        default_name
    } else {
        name.trim().to_owned()
    };
    let (id, token) = add_device(&tx, &vault, &name)?;
    tx.execute(
        "UPDATE join_codes SET used_at = ?1, used_by = ?2 WHERE code_hash = ?3",
        params![now, id, hash_secret(code)],
    )?;
    tx.commit()?;
    Ok(Some((id, token, vault, name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate_server(&mut c).unwrap();
        migrate_server(&mut c).unwrap(); // идемпотентно
        c
    }

    #[test]
    fn tokens_lifecycle() {
        let c = db();
        let (id, token) = add_device(&c, "notes", "laptop").unwrap();
        assert!(token.starts_with("ns_"));
        let a = authenticate(&c, &token).unwrap().unwrap();
        assert_eq!(
            (a.id, a.vault.as_str(), a.name.as_str()),
            (id, "notes", "laptop")
        );
        assert!(authenticate(&c, "ns_unknown").unwrap().is_none());
        assert!(authenticate(&c, "garbage").unwrap().is_none());
        // токен в открытом виде не хранится
        let stored: Vec<u8> = c
            .query_row("SELECT token_hash FROM devices WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, hash_secret(&token));
        assert_eq!(revoke_by_name(&c, Some("notes"), "laptop").unwrap(), 1);
        assert!(authenticate(&c, &token).unwrap().is_none());
    }

    #[test]
    fn vault_names_validated() {
        assert!(valid_vault_name("my-notes-2"));
        assert!(!valid_vault_name("My"));
        assert!(!valid_vault_name("../x"));
        assert!(!valid_vault_name(""));
        assert!(!valid_vault_name(&"a".repeat(65)));
        let c = db();
        assert!(add_device(&c, "Bad", "x").is_err());
    }

    #[test]
    fn join_codes_are_single_use() {
        let mut c = db();
        let (code, exp) = create_join_code(&c, "notes", "phone", None).unwrap();
        assert_eq!(code.len(), 16);
        assert!(exp > now_ms());
        assert!(peek_join_code(&c, &code).unwrap().is_some());
        let (id, token, vault, name) = redeem_join_code(&mut c, &code, "").unwrap().unwrap();
        assert_eq!((vault.as_str(), name.as_str()), ("notes", "phone"));
        assert_eq!(authenticate(&c, &token).unwrap().unwrap().id, id);
        assert!(redeem_join_code(&mut c, &code, "again").unwrap().is_none());
        assert!(peek_join_code(&c, &code).unwrap().is_none());
    }

    #[test]
    fn expired_code_rejected() {
        let mut c = db();
        let (code, _) = create_join_code(&c, "notes", "phone", None).unwrap();
        c.execute("UPDATE join_codes SET expires_at = 1", [])
            .unwrap();
        assert!(redeem_join_code(&mut c, &code, "").unwrap().is_none());
    }
}
