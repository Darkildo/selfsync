//! Настройки клиента: `.notesync/config.toml` внутри синхронизируемой папки.
//! Там же индекс, кэш базовых версий, временные файлы и (по желанию) ключ.

use std::path::{Path, PathBuf};

use anyhow::Context;
use notesync_core::engine::EngineConfig;
use serde::{Deserialize, Serialize};

/// Служебный каталог внутри папки (исключается из синка всегда).
pub const STATE_DIR: &str = ".notesync";
/// Локальная корзина — как у Obsidian, чтобы папка оставалась vault'ом.
pub const TRASH_DIR: &str = ".trash";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: String,
    pub token: String,
    pub vault: String,
    pub device_name: String,
    pub excludes: Vec<String>,
    /// Сервер работает постоянно (socket/serve): long-poll вместо частого опроса.
    pub use_wait: bool,
    pub debounce_ms: u64,
    pub poll_active_ms: u64,
    pub poll_idle_max_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: String::new(),
            token: String::new(),
            vault: String::new(),
            device_name: hostname(),
            excludes: vec![".obsidian/workspace*.json".into()],
            use_wait: false,
            debounce_ms: 2500,
            poll_active_ms: 15_000,
            poll_idle_max_ms: 300_000,
        }
    }
}

impl Config {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(STATE_DIR).join("config.toml")
    }

    pub fn load(dir: &Path) -> anyhow::Result<Config> {
        let p = Self::path(dir);
        let text = std::fs::read_to_string(&p).with_context(|| {
            format!(
                "нет настроек {}: сначала notesync-cli init или connect",
                p.display()
            )
        })?;
        toml::from_str(&text).with_context(|| format!("{} не разбирается", p.display()))
    }

    /// Сохранить (токен внутри — только владельцу).
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let p = Self::path(dir);
        std::fs::create_dir_all(p.parent().unwrap_or(dir))?;
        write_private(&p, toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn engine(&self) -> EngineConfig {
        EngineConfig {
            device_name: self.device_name.clone(),
            excludes: self.excludes.clone(),
            hard_excludes: vec![format!("{STATE_DIR}/"), format!("{TRASH_DIR}/")],
            case_insensitive: cfg!(any(target_os = "macos", target_os = "windows")),
            debounce_ms: self.debounce_ms,
            poll_active_ms: self.poll_active_ms,
            poll_idle_max_ms: self.poll_idle_max_ms,
            use_wait: self.use_wait,
            tz_offset_min: local_offset_min(),
            ..EngineConfig::default()
        }
    }
}

/// Файл с правами 0600 (токен, ключ).
pub fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("запись {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_owned())
}

/// Смещение местного времени, минуты (для дат в именах конфликтных копий).
fn local_offset_min() -> i32 {
    #[cfg(unix)]
    {
        // localtime_r знает часовой пояс системы; без внешних крейтов.
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { !libc::localtime_r(&now, &mut tm).is_null() } {
            return i32::try_from(tm.tm_gmtoff / 60).unwrap_or(0);
        }
    }
    0
}
