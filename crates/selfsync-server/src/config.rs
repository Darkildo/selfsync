//! Конфигурация сервера: переменные окружения, опционально TOML (`--config`).
//! Переменные окружения приоритетнее файла.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

pub const DEFAULT_MAX_BLOB: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_BODY: u64 = 16 * 1024 * 1024;
/// Однократная загрузка блоба (`PUT /v1/blobs/{hash}`).
pub const SINGLE_PUT_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub public_url: Option<String>,
    /// `None` — использовать значение по умолчанию для режима.
    pub idle_timeout: Option<Duration>,
    pub max_blob_size: u64,
    pub max_body_size: u64,
    pub log: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    public_url: Option<String>,
    idle_timeout: Option<String>,
    max_blob_size: Option<String>,
    max_body_size: Option<String>,
    log: Option<String>,
}

impl Config {
    /// Собирает конфигурацию: значения по умолчанию ← TOML ← окружение ← флаг `--data`.
    pub fn load(
        config_file: Option<&std::path::Path>,
        data_flag: Option<PathBuf>,
    ) -> anyhow::Result<Config> {
        let file: FileConfig = match config_file {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("не читается конфиг {}", p.display()))?;
                toml::from_str(&text)
                    .with_context(|| format!("ошибка в конфиге {}", p.display()))?
            }
            None => FileConfig::default(),
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());

        let data_dir = data_flag
            .or_else(|| env("SELFSYNC_DATA_DIR").map(PathBuf::from))
            .or(file.data_dir)
            .or_else(|| {
                env("STATE_DIRECTORY").map(|s| {
                    // systemd может передать несколько каталогов через ':'.
                    PathBuf::from(s.split(':').next().unwrap_or_default())
                })
            })
            .unwrap_or_else(|| PathBuf::from("/var/lib/selfsync"));

        let public_url = env("SELFSYNC_PUBLIC_URL")
            .or(file.public_url)
            .map(|u| u.trim_end_matches('/').to_owned());

        let idle_timeout = match env("SELFSYNC_IDLE_TIMEOUT").or(file.idle_timeout) {
            Some(s) => Some(parse_duration(&s)?),
            None => None,
        };
        let max_blob_size = match env("SELFSYNC_MAX_BLOB_SIZE").or(file.max_blob_size) {
            Some(s) => parse_size(&s)?,
            None => DEFAULT_MAX_BLOB,
        };
        let max_body_size = match env("SELFSYNC_MAX_BODY_SIZE").or(file.max_body_size) {
            Some(s) => parse_size(&s)?,
            None => DEFAULT_MAX_BODY,
        };
        let log = env("SELFSYNC_LOG")
            .or(file.log)
            .unwrap_or_else(|| "info".to_owned());
        Ok(Config {
            data_dir,
            public_url,
            idle_timeout,
            max_blob_size,
            max_body_size,
            log,
        })
    }

    /// Конфигурация для тестов.
    pub fn for_tests(data_dir: PathBuf) -> Config {
        Config {
            data_dir,
            public_url: None,
            idle_timeout: None,
            max_blob_size: DEFAULT_MAX_BLOB,
            max_body_size: DEFAULT_MAX_BODY,
            log: "warn".to_owned(),
        }
    }
}

/// `humantime`, плюс `0` = выключено.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    humantime::parse_duration(s).with_context(|| format!("неверная длительность: {s}"))
}

/// Размер: `512MiB`, `16MB`, `1024`, `8 KiB`.
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num
        .parse()
        .with_context(|| format!("неверный размер: {s}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1000,
        "kib" => 1024,
        "m" | "mb" => 1000 * 1000,
        "mib" => 1024 * 1024,
        "g" | "gb" => 1000 * 1000 * 1000,
        "gib" => 1024 * 1024 * 1024,
        other => bail!("неизвестная единица размера: {other}"),
    };
    n.checked_mul(mult).context("размер слишком велик")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("16 MB").unwrap(), 16_000_000);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert!(parse_size("12XB").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert!(parse_duration("abc").is_err());
    }
}
