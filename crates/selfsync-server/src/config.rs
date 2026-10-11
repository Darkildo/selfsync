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
    pub rate: RateConfig,
    /// Доверять `X-Forwarded-For` (адрес клиента для лимитера).
    pub trusted_proxy: bool,
}

/// Порог неудач за окно и блокировка после него. `attempts == 0` — лимит выключен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRule {
    pub attempts: u32,
    pub window: Duration,
    pub block: Duration,
}

/// Лимиты неудачных попыток (`throttle.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateConfig {
    /// `redeem:<адрес>`: коды подключения с одного адреса.
    pub redeem: RateRule,
    /// `redeem:*`: со всех адресов за то же окно (`0` — выключено).
    pub redeem_global: u32,
    /// `auth:<адрес>`: ответы 401 на предъявленный токен.
    pub auth: RateRule,
}

impl Default for RateConfig {
    fn default() -> Self {
        RateConfig {
            redeem: RateRule {
                attempts: 10,
                window: Duration::from_secs(15 * 60),
                block: Duration::from_secs(15 * 60),
            },
            redeem_global: 200,
            auth: RateRule {
                attempts: 30,
                window: Duration::from_secs(60),
                block: Duration::from_secs(60),
            },
        }
    }
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
    trusted_proxy: Option<bool>,
    #[serde(default)]
    rate: FileRate,
}

/// Секция `[rate]`: те же ключи, что `SELFSYNC_RATE_*`, в нижнем регистре.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRate {
    redeem_attempts: Option<u32>,
    redeem_window: Option<String>,
    redeem_block: Option<String>,
    redeem_global: Option<u32>,
    auth_attempts: Option<u32>,
    auth_window: Option<String>,
    auth_block: Option<String>,
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
        let trusted_proxy = match env("SELFSYNC_TRUSTED_PROXY") {
            Some(s) => parse_bool("SELFSYNC_TRUSTED_PROXY", &s)?,
            None => file.trusted_proxy.unwrap_or(false),
        };
        let rate = load_rate(&env, file.rate)?;
        Ok(Config {
            data_dir,
            public_url,
            idle_timeout,
            max_blob_size,
            max_body_size,
            log,
            rate,
            trusted_proxy,
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
            rate: RateConfig::default(),
            trusted_proxy: false,
        }
    }
}

fn load_rate(env: &dyn Fn(&str) -> Option<String>, file: FileRate) -> anyhow::Result<RateConfig> {
    let d = RateConfig::default();
    let count = |name: &str, f: Option<u32>, def: u32| -> anyhow::Result<u32> {
        match env(name) {
            Some(s) => s
                .trim()
                .parse()
                .with_context(|| format!("{name}: нужно целое число ≥ 0, получено «{s}»")),
            None => Ok(f.unwrap_or(def)),
        }
    };
    // Окно и блокировка — не меньше секунды: `0` здесь почти наверняка опечатка,
    // а выключается лимит порогом.
    let span = |name: &str, f: Option<String>, def: Duration| -> anyhow::Result<Duration> {
        let Some(s) = env(name).or(f) else {
            return Ok(def);
        };
        let v = parse_duration(&s).with_context(|| name.to_owned())?;
        if v < Duration::from_secs(1) {
            bail!("{name}: не меньше секунды (выключить лимит — порогом 0), получено «{s}»");
        }
        Ok(v)
    };
    Ok(RateConfig {
        redeem: RateRule {
            attempts: count(
                "SELFSYNC_RATE_REDEEM_ATTEMPTS",
                file.redeem_attempts,
                d.redeem.attempts,
            )?,
            window: span(
                "SELFSYNC_RATE_REDEEM_WINDOW",
                file.redeem_window,
                d.redeem.window,
            )?,
            block: span(
                "SELFSYNC_RATE_REDEEM_BLOCK",
                file.redeem_block,
                d.redeem.block,
            )?,
        },
        redeem_global: count(
            "SELFSYNC_RATE_REDEEM_GLOBAL",
            file.redeem_global,
            d.redeem_global,
        )?,
        auth: RateRule {
            attempts: count(
                "SELFSYNC_RATE_AUTH_ATTEMPTS",
                file.auth_attempts,
                d.auth.attempts,
            )?,
            window: span("SELFSYNC_RATE_AUTH_WINDOW", file.auth_window, d.auth.window)?,
            block: span("SELFSYNC_RATE_AUTH_BLOCK", file.auth_block, d.auth.block)?,
        },
    })
}

fn parse_bool(name: &str, s: &str) -> anyhow::Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{name}: ожидается 1 или 0, получено «{s}»"),
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

    fn rate_from(pairs: &[(&str, &str)], file: &str) -> anyhow::Result<RateConfig> {
        let env = |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        };
        let file: FileConfig = toml::from_str(file).unwrap();
        load_rate(&env, file.rate)
    }

    #[test]
    fn rate_settings() {
        assert_eq!(rate_from(&[], "").unwrap(), RateConfig::default());
        let r = rate_from(
            &[("SELFSYNC_RATE_AUTH_WINDOW", "5m")],
            "[rate]\nredeem_attempts = 0\nauth_window = \"2m\"\nauth_block = \"30s\"",
        )
        .unwrap();
        assert_eq!(r.redeem.attempts, 0);
        assert_eq!(r.auth.window, Duration::from_secs(300)); // окружение приоритетнее
        assert_eq!(r.auth.block, Duration::from_secs(30));
        let e = rate_from(&[("SELFSYNC_RATE_REDEEM_BLOCK", "500ms")], "").unwrap_err();
        assert!(format!("{e:#}").contains("не меньше секунды"));
        assert!(rate_from(&[("SELFSYNC_RATE_REDEEM_GLOBAL", "-1")], "").is_err());
        assert!(toml::from_str::<FileConfig>("[rate]\nbogus = 1").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert!(parse_duration("abc").is_err());
    }
}
