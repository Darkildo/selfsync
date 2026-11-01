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
/// Сколько хранится обмен ключом после входа нового устройства по коду.
pub const HANDOFF_TTL_MS: i64 = 10 * 60 * 1000;
/// `last_seen` обновляется не чаще раза в минуту.
const LAST_SEEN_STEP_MS: i64 = 60 * 1000;

const MIGRATIONS: &[&str] = &[
    "
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
",
    "
CREATE TABLE throttle (
  key           TEXT PRIMARY KEY,
  window_start  INTEGER NOT NULL,
  failures      INTEGER NOT NULL,
  blocked_until INTEGER
);
",
    "
ALTER TABLE devices ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
ALTER TABLE join_codes ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
",
    "
ALTER TABLE devices ADD COLUMN client TEXT NOT NULL DEFAULT '';
",
    "
-- Счётчики событий для /metrics: переживают выход сервера по простою.
CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
",
    "
-- Передача ключа по коду (1.4): обязательство ключа источника, ключ и MAC нового
-- устройства (оно — used_by), раскрытый ключ источника и обёртка мастер-ключа.
ALTER TABLE join_codes ADD COLUMN offer_commit BLOB;
ALTER TABLE join_codes ADD COLUMN joiner_pk BLOB;
ALTER TABLE join_codes ADD COLUMN joiner_mac BLOB;
ALTER TABLE join_codes ADD COLUMN owner_pk BLOB;
ALTER TABLE join_codes ADD COLUMN wrapped BLOB;
CREATE INDEX join_codes_used_by ON join_codes(used_by);
",
];

/// Увеличить счётчик событий.
pub fn bump_counter(c: &Connection, name: &str) -> rusqlite::Result<()> {
    c.prepare_cached(
        "INSERT INTO counters (name, value) VALUES (?1, 1)
         ON CONFLICT(name) DO UPDATE SET value = value + 1",
    )?
    .execute(params![name])?;
    Ok(())
}

pub fn counters(c: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut st = c.prepare("SELECT name, value FROM counters ORDER BY name")?;
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
}

/// Ключи лимитера, заблокированные сейчас, по правилу (`auth`, `redeem`).
pub fn blocked_now(c: &Connection, now: i64) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut st = c.prepare(
        "SELECT substr(key, 1, instr(key, ':') - 1) AS rule, COUNT(*) FROM throttle
         WHERE blocked_until > ?1 AND instr(key, ':') > 0 GROUP BY rule ORDER BY rule",
    )?;
    st.query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// Длина строки клиента в БД.
const CLIENT_MAX: usize = 64;

/// Заголовок `x-selfsync-client` для хранения: только печатный ASCII, не длиннее
/// [`CLIENT_MAX`]. Данные не доверенные — только для показа админу и устройствам.
pub fn client_label(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(CLIENT_MAX)
        .collect()
}

/// Запомнить, каким клиентом подключено устройство.
pub fn set_client(c: &Connection, id: u32, client: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE devices SET client = ?1 WHERE id = ?2",
        params![client, id],
    )?;
    Ok(())
}

pub fn migrate_server(c: &mut Connection) -> rusqlite::Result<()> {
    migrate(c, MIGRATIONS)
}

/// Аутентифицированное устройство.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    pub id: u32,
    pub vault: String,
    pub name: String,
    /// Только GET и HEAD.
    pub read_only: bool,
    /// Последний известный клиент (`plugin/0.3.0`); пусто — неизвестен.
    pub client: String,
}

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: u32,
    pub vault: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub revoked: bool,
    pub read_only: bool,
    pub client: String,
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

/// Создаёт устройство и возвращает (id, токен). `max_devices` — лимит активных
/// устройств vault'а (`0` — без лимита).
pub fn add_device(
    c: &Connection,
    vault: &str,
    name: &str,
    read_only: bool,
    max_devices: u32,
) -> anyhow::Result<(u32, String)> {
    anyhow::ensure!(
        valid_vault_name(vault),
        "имя vault'а должно быть [a-z0-9-]{{1,64}}"
    );
    check_device_limit(c, vault, max_devices)?;
    let name = name.trim();
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 128,
        "имя устройства: 1–128 байт"
    );
    let token = generate_token()?;
    c.execute(
        "INSERT INTO devices (vault, name, token_hash, created_at, read_only)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![vault, name, hash_secret(&token), now_ms(), read_only],
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
    #[allow(clippy::type_complexity)]
    let row: Option<(i64, String, String, Vec<u8>, i64, bool, String)> = c
        .query_row(
            "SELECT id, vault, name, token_hash, last_seen, read_only, client FROM devices
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![h],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((id, vault, name, stored, last_seen, read_only, client)) = row else {
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
        read_only,
        client,
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
        read_only: r.get(6)?,
        client: r.get(7)?,
    })
}

pub fn list_devices(c: &Connection, vault: Option<&str>) -> rusqlite::Result<Vec<DeviceRow>> {
    let mut out = Vec::new();
    match vault {
        Some(v) => {
            let mut st = c.prepare(
                "SELECT id, vault, name, created_at, last_seen, revoked_at, read_only, client FROM devices
                 WHERE vault = ?1 ORDER BY id",
            )?;
            for d in st.query_map(params![v], row_to_device)? {
                out.push(d?);
            }
        }
        None => {
            let mut st = c.prepare(
                "SELECT id, vault, name, created_at, last_seen, revoked_at, read_only, client FROM devices
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

/// Лимит активных устройств vault'а не исчерпан (`0` — без лимита).
pub fn check_device_limit(c: &Connection, vault: &str, max_devices: u32) -> anyhow::Result<()> {
    if max_devices > 0 && count_devices(c, vault)? >= max_devices {
        return Err(crate::quota::DeviceLimit(max_devices).into());
    }
    Ok(())
}

/// Создаёт одноразовый код подключения. Возвращает (код, истекает_в).
pub fn create_join_code(
    c: &Connection,
    vault: &str,
    name: &str,
    created_by: Option<u32>,
    read_only: bool,
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
        "INSERT INTO join_codes
           (code_hash, vault, name, created_by, created_at, expires_at, read_only)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            hash_secret(&code),
            vault,
            name.trim(),
            created_by,
            now,
            expires,
            read_only
        ],
    )?;
    Ok((code, expires))
}

/// Действующий код подключения: vault, имя будущего устройства, только чтение.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinInfo {
    pub vault: String,
    pub name: String,
    pub read_only: bool,
}

/// Состояние кода без его использования (для HTML-страницы).
pub fn peek_join_code(c: &Connection, code: &str) -> rusqlite::Result<Option<JoinInfo>> {
    c.query_row(
        "SELECT vault, name, read_only FROM join_codes
         WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
        params![hash_secret(code), now_ms()],
        |r| {
            Ok(JoinInfo {
                vault: r.get(0)?,
                name: r.get(1)?,
                read_only: r.get(2)?,
            })
        },
    )
    .optional()
}

/// Обменивает код на новое устройство. Код одноразовый: повтор и истёкший — `None`.
/// Действующий код подключения выдаёт устройство только для чтения: `None` —
/// кода нет (неизвестен, истёк или использован).
pub fn join_code_read_only(c: &Connection, code: &str) -> anyhow::Result<Option<bool>> {
    Ok(c.query_row(
        "SELECT read_only FROM join_codes
         WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
        params![hash_secret(code), now_ms()],
        |r| r.get(0),
    )
    .optional()?)
}

pub fn redeem_join_code(
    c: &mut Connection,
    code: &str,
    name: &str,
    max_devices: &dyn Fn(&str) -> anyhow::Result<u32>,
) -> anyhow::Result<Option<(u32, String, String, String)>> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();
    let row: Option<(String, String, bool)> = tx
        .query_row(
            "SELECT vault, name, read_only FROM join_codes
             WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
            params![hash_secret(code), now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((vault, default_name, read_only)) = row else {
        return Ok(None);
    };
    let name = if name.trim().is_empty() {
        default_name
    } else {
        name.trim().to_owned()
    };
    let (id, token) = add_device(&tx, &vault, &name, read_only, max_devices(&vault)?)?;
    tx.execute(
        "UPDATE join_codes SET used_at = ?1, used_by = ?2 WHERE code_hash = ?3",
        params![now, id, hash_secret(code)],
    )?;
    // Источник передаёт ключ: обмену — ещё HANDOFF_TTL_MS с этой минуты.
    tx.execute(
        "UPDATE join_codes SET expires_at = ?1 WHERE code_hash = ?2 AND offer_commit IS NOT NULL",
        params![now + HANDOFF_TTL_MS, hash_secret(code)],
    )?;
    tx.commit()?;
    Ok(Some((id, token, vault, name)))
}

/// Источник передаст ключ по этому коду: обязательство его ключа обмена.
pub fn offer_handoff(c: &Connection, code: &str, commit: &[u8]) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE join_codes SET offer_commit = ?1 WHERE code_hash = ?2 AND used_at IS NULL",
        params![commit, hash_secret(code)],
    )?;
    Ok(())
}

/// Обмен ключом, в котором `device` — новое устройство: обязательство источника.
/// `None` — обмена нет или он истёк.
pub fn joiner_commit(c: &Connection, device: u32) -> rusqlite::Result<Option<Vec<u8>>> {
    c.query_row(
        "SELECT offer_commit FROM join_codes
         WHERE used_by = ?1 AND offer_commit IS NOT NULL AND expires_at > ?2",
        params![device, now_ms()],
        |r| r.get(0),
    )
    .optional()
}

/// Новое устройство прислало свой ключ обмена и MAC (из QR). Пишется один раз;
/// `false` — обмена нет, он истёк или ключ уже прислан.
pub fn joiner_handoff_put(
    c: &Connection,
    device: u32,
    pk: &[u8],
    mac: Option<&[u8]>,
) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "UPDATE join_codes SET joiner_pk = ?1, joiner_mac = ?2
         WHERE used_by = ?3 AND offer_commit IS NOT NULL AND expires_at > ?4
           AND joiner_pk IS NULL",
        params![pk, mac, device, now_ms()],
    )? > 0)
}

/// Состояние обмена ключом по коду.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// Имя нового устройства (или предложенное в коде, пока оно не вошло).
    pub name: String,
    pub joiner_pk: Option<Vec<u8>>,
    pub joiner_mac: Option<Vec<u8>>,
    pub owner_pk: Option<Vec<u8>>,
    pub wrapped: Option<Vec<u8>>,
}

/// Отказ шага источника.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffRefusal {
    /// Кода нет, он истёк, без обмена или его создало другое устройство.
    NotFound,
    /// Раскрытый ключ не совпал с обязательством.
    BadCommit,
}

/// Шаг источника (устройство `device` vault'а `vault`, создавшее код): раскрыть
/// свой ключ (`owner_pk` — когда новое устройство уже прислало свой, и только тот,
/// которым источник обязался) и отдать обёртку (`wrapped` — когда ключ раскрыт).
/// Каждое поле пишется один раз, повтор молча не меняет записанного.
pub fn owner_handoff(
    c: &Connection,
    vault: &str,
    device: u32,
    code: &str,
    owner_pk: Option<&[u8]>,
    wrapped: Option<&[u8]>,
) -> rusqlite::Result<Result<Handoff, HandoffRefusal>> {
    let h = hash_secret(code);
    let now = now_ms();
    let row: Option<(Vec<u8>, String, Option<u32>)> = c
        .query_row(
            "SELECT offer_commit, name, used_by FROM join_codes
             WHERE code_hash = ?1 AND vault = ?2 AND created_by = ?3 AND expires_at > ?4
               AND offer_commit IS NOT NULL",
            params![h, vault, device, now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((commit, code_name, used_by)) = row else {
        return Ok(Err(HandoffRefusal::NotFound));
    };
    if let Some(pk) = owner_pk {
        if Sha256::digest(pk).as_slice() != commit.as_slice() {
            return Ok(Err(HandoffRefusal::BadCommit));
        }
        c.execute(
            "UPDATE join_codes SET owner_pk = ?1
             WHERE code_hash = ?2 AND joiner_pk IS NOT NULL AND owner_pk IS NULL",
            params![pk, h],
        )?;
    }
    if let Some(w) = wrapped {
        c.execute(
            "UPDATE join_codes SET wrapped = ?1
             WHERE code_hash = ?2 AND owner_pk IS NOT NULL AND wrapped IS NULL",
            params![w, h],
        )?;
    }
    // Имя — то, под которым устройство вошло (его могли поменять при подключении).
    let name: Option<String> = match used_by {
        Some(id) => c
            .query_row("SELECT name FROM devices WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?,
        None => None,
    };
    let mut out = c.query_row(
        "SELECT joiner_pk, joiner_mac, owner_pk, wrapped FROM join_codes WHERE code_hash = ?1",
        params![h],
        |r| {
            Ok(Handoff {
                name: String::new(),
                joiner_pk: r.get(0)?,
                joiner_mac: r.get(1)?,
                owner_pk: r.get(2)?,
                wrapped: r.get(3)?,
            })
        },
    )?;
    out.name = name.unwrap_or(code_name);
    Ok(Ok(out))
}

/// Шаг нового устройства: ключ источника и обёртка, когда они есть. Обёртка
/// отдаётся и повторно (ответ мог потеряться; без секрета нового устройства она
/// бесполезна), пока устройство не закроет обмен ([`joiner_handoff_delete`]) или
/// он не истечёт. `None` — обмена нет.
pub fn joiner_handoff_get(c: &Connection, device: u32) -> rusqlite::Result<Option<Handoff>> {
    c.query_row(
        "SELECT owner_pk, wrapped FROM join_codes
         WHERE used_by = ?1 AND offer_commit IS NOT NULL AND expires_at > ?2",
        params![device, now_ms()],
        |r| {
            Ok(Handoff {
                owner_pk: r.get(0)?,
                wrapped: r.get(1)?,
                ..Handoff::default()
            })
        },
    )
    .optional()
}

/// Новое устройство закрыло обмен (ключ получен или обмен прерван): запись стёрта
/// вместе с ключами и обёрткой. `false` — обмена не было.
pub fn joiner_handoff_delete(c: &Connection, device: u32) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "DELETE FROM join_codes WHERE used_by = ?1 AND offer_commit IS NOT NULL",
        params![device],
    )? > 0)
}

/// Истёкшие коды и обмены ключом (`selfsync sweep`): открытые ключи и обёртки не
/// лежат дольше [`HANDOFF_TTL_MS`]. Возвращает число удалённых строк.
pub fn sweep_join_codes(c: &Connection, now: i64) -> rusqlite::Result<usize> {
    c.execute(
        "DELETE FROM join_codes WHERE expires_at <= ?1",
        params![now],
    )
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
        let (id, token) = add_device(&c, "notes", "laptop", false, 0).unwrap();
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
        assert!(add_device(&c, "Bad", "x", false, 0).is_err());
    }

    #[test]
    fn join_codes_are_single_use() {
        let mut c = db();
        let (code, exp) = create_join_code(&c, "notes", "phone", None, false).unwrap();
        assert_eq!(code.len(), 16);
        assert!(exp > now_ms());
        assert!(peek_join_code(&c, &code).unwrap().is_some());
        let (id, token, vault, name) = redeem_join_code(&mut c, &code, "", &|_| Ok(0))
            .unwrap()
            .unwrap();
        assert_eq!((vault.as_str(), name.as_str()), ("notes", "phone"));
        let a = authenticate(&c, &token).unwrap().unwrap();
        assert_eq!((a.id, a.read_only), (id, false));
        assert!(
            redeem_join_code(&mut c, &code, "again", &|_| Ok(0))
                .unwrap()
                .is_none()
        );
        assert!(peek_join_code(&c, &code).unwrap().is_none());
    }

    #[test]
    fn handoff_steps_once_in_order() {
        let mut c = db();
        let (src, _) = add_device(&c, "notes", "laptop", false, 0).unwrap();
        let (other, _) = add_device(&c, "notes", "tablet", false, 0).unwrap();
        let (code, _) = create_join_code(&c, "notes", "phone", Some(src), false).unwrap();
        let owner = b"owner-key";
        let commit: [u8; 32] = Sha256::digest(owner).into();
        offer_handoff(&c, &code, &commit).unwrap();
        // До входа нового устройства раскрывать нечего.
        let h = owner_handoff(&c, "notes", src, &code, Some(owner), Some(b"early"))
            .unwrap()
            .unwrap();
        assert_eq!((h.joiner_pk, h.owner_pk, h.wrapped), (None, None, None));
        assert_eq!(h.name, "phone");
        let (joiner, ..) = redeem_join_code(&mut c, &code, "my phone", &|_| Ok(0))
            .unwrap()
            .unwrap();
        assert_eq!(joiner_commit(&c, joiner).unwrap(), Some(commit.to_vec()));
        assert_eq!(joiner_commit(&c, src).unwrap(), None);
        assert!(joiner_handoff_put(&c, joiner, b"joiner", Some(b"mac")).unwrap());
        assert!(
            !joiner_handoff_put(&c, joiner, b"again", None).unwrap(),
            "один раз"
        );
        assert_eq!(
            owner_handoff(&c, "notes", other, &code, None, None).unwrap(),
            Err(HandoffRefusal::NotFound),
            "код создало другое устройство"
        );
        assert_eq!(
            owner_handoff(&c, "other", src, &code, None, None).unwrap(),
            Err(HandoffRefusal::NotFound),
        );
        assert_eq!(
            owner_handoff(&c, "notes", src, &code, Some(b"not-committed"), None).unwrap(),
            Err(HandoffRefusal::BadCommit),
        );
        // Обёртка — только после раскрытия ключа.
        let h = owner_handoff(&c, "notes", src, &code, None, Some(b"early"))
            .unwrap()
            .unwrap();
        assert_eq!(h.name, "my phone");
        assert_eq!(
            (h.joiner_pk.as_deref(), h.joiner_mac.as_deref(), h.wrapped),
            (Some(&b"joiner"[..]), Some(&b"mac"[..]), None)
        );
        let j = joiner_handoff_get(&c, joiner).unwrap().unwrap();
        assert_eq!((j.owner_pk, j.wrapped), (None, None));
        let h = owner_handoff(&c, "notes", src, &code, Some(owner), Some(b"wrap"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (h.owner_pk.as_deref(), h.wrapped.as_deref()),
            (Some(&owner[..]), Some(&b"wrap"[..]))
        );
        let h = owner_handoff(&c, "notes", src, &code, None, Some(b"other"))
            .unwrap()
            .unwrap();
        assert_eq!(
            h.wrapped.as_deref(),
            Some(&b"wrap"[..]),
            "обёртка пишется один раз"
        );
        // Обёртка отдаётся повторно (ответ мог потеряться), пока устройство не
        // закроет обмен.
        let j = joiner_handoff_get(&c, joiner).unwrap().unwrap();
        assert_eq!(j.wrapped.as_deref(), Some(&b"wrap"[..]));
        assert_eq!(joiner_handoff_get(&c, joiner).unwrap().unwrap(), j);
        assert!(
            !joiner_handoff_delete(&c, src).unwrap(),
            "чужой обмен не стереть"
        );
        assert!(joiner_handoff_delete(&c, joiner).unwrap());
        assert!(!joiner_handoff_delete(&c, joiner).unwrap());
        assert_eq!(joiner_handoff_get(&c, joiner).unwrap(), None);
        assert_eq!(
            owner_handoff(&c, "notes", src, &code, None, None).unwrap(),
            Err(HandoffRefusal::NotFound)
        );
    }

    #[test]
    fn handoff_expires_and_is_swept() {
        let mut c = db();
        let (src, _) = add_device(&c, "notes", "laptop", false, 0).unwrap();
        // Код без обязательства обмена не начинает.
        let (plain, _) = create_join_code(&c, "notes", "a", Some(src), false).unwrap();
        let (id, ..) = redeem_join_code(&mut c, &plain, "", &|_| Ok(0))
            .unwrap()
            .unwrap();
        assert_eq!(joiner_commit(&c, id).unwrap(), None);
        assert!(!joiner_handoff_put(&c, id, b"pk", None).unwrap());
        assert_eq!(joiner_handoff_get(&c, id).unwrap(), None);
        let (code, _) = create_join_code(&c, "notes", "b", Some(src), false).unwrap();
        offer_handoff(&c, &code, &[1; 32]).unwrap();
        let (id, ..) = redeem_join_code(&mut c, &code, "", &|_| Ok(0))
            .unwrap()
            .unwrap();
        joiner_handoff_put(&c, id, b"pk", None).unwrap();
        let exp: i64 = c
            .query_row(
                "SELECT expires_at FROM join_codes WHERE used_by = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exp > now_ms() + HANDOFF_TTL_MS - 60_000 && exp <= now_ms() + HANDOFF_TTL_MS);
        assert_eq!(
            sweep_join_codes(&c, now_ms()).unwrap(),
            0,
            "живой обмен не трогается"
        );
        assert_eq!(
            sweep_join_codes(&c, exp).unwrap(),
            1,
            "истёк — стёрт с ключами"
        );
        assert_eq!(joiner_commit(&c, id).unwrap(), None);
    }

    #[test]
    fn read_only_flag_travels_from_code_to_device() {
        let mut c = db();
        let (_, t) = add_device(&c, "notes", "nas", true, 0).unwrap();
        assert!(authenticate(&c, &t).unwrap().unwrap().read_only);
        let (code, _) = create_join_code(&c, "notes", "tablet", None, true).unwrap();
        assert!(peek_join_code(&c, &code).unwrap().unwrap().read_only);
        let (id, token, _, _) = redeem_join_code(&mut c, &code, "", &|_| Ok(0))
            .unwrap()
            .unwrap();
        assert!(authenticate(&c, &token).unwrap().unwrap().read_only);
        let rows = list_devices(&c, Some("notes")).unwrap();
        assert!(rows.iter().all(|d| d.read_only));
        assert!(rows.iter().any(|d| d.id == id));
    }

    #[test]
    fn expired_code_rejected() {
        let mut c = db();
        let (code, _) = create_join_code(&c, "notes", "phone", None, false).unwrap();
        c.execute("UPDATE join_codes SET expires_at = 1", [])
            .unwrap();
        assert!(
            redeem_join_code(&mut c, &code, "", &|_| Ok(0))
                .unwrap()
                .is_none()
        );
    }
}
