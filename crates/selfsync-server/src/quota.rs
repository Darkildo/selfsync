//! Квоты (F16): байты хранилища vault'а (мягкий и жёсткий порог), число устройств,
//! размер файла; общий потолок байтов сервера. Лимиты vault'а лежат в его `meta`
//! поверх умолчаний конфигурации; `0` — без ограничения.
//!
//! Байты — размер всех блобов vault'а на диске: текущие версии, история, корзина и
//! ещё не убранные `gc` блобы без ссылок. Счётчик `usage_bytes` в `meta` меняется при
//! сохранении и удалении блоба, `verify` пересчитывает его по диску (между записью
//! файла и счётчика процесс мог упасть).

use std::sync::Arc;

use http::StatusCode;
use rusqlite::{Connection, params};
use selfsync_core::hash::Hash;
use selfsync_proto::v1 as pb;

use crate::blobs::BlobStore;
use crate::config::{Config, Limits};
use crate::db::vault::{meta_int, set_meta_int};
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Vault, list_vault_dirs};

const SOFT: &str = "quota_soft";
const HARD: &str = "quota_hard";
const DEVICES: &str = "quota_devices";
const MAX_FILE: &str = "quota_max_file";
const USAGE: &str = "usage_bytes";

/// Свои лимиты vault'а; `None` — действует умолчание конфигурации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Own {
    pub soft: Option<u64>,
    pub hard: Option<u64>,
    pub devices: Option<u32>,
    pub max_file: Option<u64>,
}

/// Лимит устройств vault'а исчерпан.
#[derive(Debug, thiserror::Error)]
#[error("в vault'е уже {0} устройств — это его лимит; отзовите ненужное")]
pub struct DeviceLimit(pub u32);

fn get(c: &Connection, key: &str) -> rusqlite::Result<Option<u64>> {
    Ok(meta_int(c, key)?.map(|v| u64::try_from(v).unwrap_or(0)))
}

fn put(c: &Connection, key: &str, v: Option<u64>) -> rusqlite::Result<()> {
    match v {
        Some(v) => set_meta_int(c, key, i64::try_from(v).unwrap_or(i64::MAX)),
        None => {
            c.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
            Ok(())
        }
    }
}

pub fn own(c: &Connection) -> rusqlite::Result<Own> {
    Ok(Own {
        soft: get(c, SOFT)?,
        hard: get(c, HARD)?,
        devices: get(c, DEVICES)?.map(|v| u32::try_from(v).unwrap_or(u32::MAX)),
        max_file: get(c, MAX_FILE)?,
    })
}

pub fn set_own(c: &Connection, o: &Own) -> rusqlite::Result<()> {
    put(c, SOFT, o.soft)?;
    put(c, HARD, o.hard)?;
    put(c, DEVICES, o.devices.map(u64::from))?;
    put(c, MAX_FILE, o.max_file)
}

/// Действующие лимиты vault'а.
pub fn limits(c: &Connection, defaults: &Limits) -> rusqlite::Result<Limits> {
    let o = own(c)?;
    Ok(Limits {
        soft: o.soft.unwrap_or(defaults.soft),
        hard: o.hard.unwrap_or(defaults.hard),
        devices: o.devices.unwrap_or(defaults.devices),
        max_file: o.max_file.unwrap_or(defaults.max_file),
    })
}

/// Наибольший файл vault'а: лимит vault'а, но не больше `max_blob_size` сервера.
pub fn max_file(l: &Limits, config: &Config) -> u64 {
    match l.max_file {
        0 => config.max_blob_size,
        n => n.min(config.max_blob_size),
    }
}

/// Занято блобами vault'а; `0`, пока счётчик не заведён ([`init_usage`]).
pub fn usage(c: &Connection) -> rusqlite::Result<u64> {
    Ok(get(c, USAGE)?.unwrap_or(0))
}

/// Заводит счётчик по диску, если его ещё нет (vault старше квот или новый).
pub fn init_usage(c: &Connection, store: &BlobStore) -> anyhow::Result<()> {
    if meta_int(c, USAGE)?.is_none() {
        let (bytes, _) = store.total_size()?;
        c.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params![USAGE, i64::try_from(bytes).unwrap_or(i64::MAX)],
        )?;
    }
    Ok(())
}

/// Блоб сохранён (`delta > 0`) или удалён (`< 0`).
pub fn add_usage(c: &Connection, delta: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE meta SET value = MAX(0, value + ?1) WHERE key = ?2",
        params![delta, USAGE],
    )?;
    Ok(())
}

pub fn set_usage(c: &Connection, bytes: u64) -> rusqlite::Result<()> {
    set_meta_int(c, USAGE, i64::try_from(bytes).unwrap_or(i64::MAX))
}

/// Квота в `VaultState`.
pub fn fill_state(c: &Connection, config: &Config, s: &mut pb::VaultState) -> rusqlite::Result<()> {
    let l = limits(c, &config.quota.defaults)?;
    s.used_bytes = usage(c)?;
    s.quota_soft = l.soft;
    s.quota_hard = l.hard;
    s.max_file_size = max_file(&l, config);
    Ok(())
}

/// Занято всеми vault'ами сервера.
pub fn server_usage(st: &AppState) -> ApiResult<u64> {
    let mut total = 0u64;
    for name in list_vault_dirs(&st.config) {
        let v = st.vault(&name)?;
        let c = v.pool.get()?;
        total = total.saturating_add(usage(&c)?);
    }
    Ok(total)
}

fn exceeded(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded", message)
}

/// Пределы для записи блоба, размер которого заранее неизвестен.
#[derive(Debug, Clone, Copy)]
pub struct Room {
    /// Сколько байт ещё умещается в квоту.
    pub quota: u64,
    /// Наибольший файл vault'а.
    pub max_file: u64,
}

/// Можно ли сохранить новый блоб размера `size`. Блоб, который сервер сам просит у
/// устройств (F12), принимается всегда: это починка, а не новые данные.
pub async fn check_new_blob(st: &AppState, v: &Arc<Vault>, h: Hash, size: u64) -> ApiResult<Room> {
    let defaults = st.config.quota.defaults;
    let (wanted, l, used) = v
        .db(move |c, _| {
            let wanted = crate::db::vault::is_wanted(c, &h)?;
            Ok((wanted, limits(c, &defaults)?, usage(c)?))
        })
        .await?;
    if wanted {
        return Ok(Room {
            quota: u64::MAX,
            max_file: u64::MAX,
        });
    }
    let max_file = max_file(&l, &st.config);
    if size > max_file {
        return Err(ApiError::too_large("файл больше лимита vault'а"));
    }
    let mut room = u64::MAX;
    if l.hard > 0 {
        if used.saturating_add(size) > l.hard || used >= l.hard {
            return Err(vault_full());
        }
        room = l.hard - used;
    }
    let server_hard = st.config.quota.server_hard;
    if server_hard > 0 {
        let st = st.clone();
        let total = tokio::task::spawn_blocking(move || server_usage(&st)).await??;
        if total.saturating_add(size) > server_hard || total >= server_hard {
            return Err(exceeded("место на сервере закончилось"));
        }
        room = room.min(server_hard - total);
    }
    Ok(Room {
        quota: room,
        max_file,
    })
}

pub fn vault_full() -> ApiError {
    exceeded("место vault'а закончилось")
}

/// Лимит устройств vault'а (для `add_device`).
pub fn device_limit(st: &AppState, vault: &str) -> anyhow::Result<u32> {
    let v = st.vault(vault).map_err(|e| anyhow::anyhow!(e.message))?;
    let c = v.pool.get()?;
    Ok(limits(&c, &st.config.quota.defaults)?.devices)
}
