//! `selfsync` — сервер синхронизации заметок.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use selfsync_server::config::parse_duration;
use selfsync_server::gc::GcOptions;
use selfsync_server::modes::{self, cgi};
use selfsync_server::{AppState, Config, Mode, cmd, init_logging};

#[derive(Parser)]
#[command(
    name = "selfsync",
    version,
    about = "Self-hosted сервер синхронизации заметок"
)]
struct Cli {
    /// Каталог данных (по умолчанию SELFSYNC_DATA_DIR, $STATE_DIRECTORY или /var/lib/selfsync).
    #[arg(long, global = true)]
    data: Option<PathBuf>,
    /// TOML-конфиг (переменные окружения приоритетнее).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Обычный процесс (Docker, разработка).
    Serve {
        /// `127.0.0.1:8085` или `unix:/path/to.sock`.
        #[arg(long, default_value = "127.0.0.1:8085")]
        listen: String,
        /// Выход после простоя (`0` — выключено), например `10m`.
        #[arg(long)]
        idle_timeout: Option<String>,
    },
    /// systemd socket activation с выходом по простою.
    Socket {
        #[arg(long)]
        idle_timeout: Option<String>,
    },
    /// Один CGI-запрос (включается и автоматически по GATEWAY_INTERFACE).
    Cgi,
    /// Привести схемы БД к текущей версии.
    Migrate {
        #[arg(long)]
        vault: Option<String>,
    },
    /// Vault'ы.
    #[command(subcommand)]
    Vault(VaultCmd),
    /// Токены устройств.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Одноразовая ссылка подключения первого устройства (и QR в терминале).
    Link {
        #[arg(long)]
        vault: String,
        /// Внешний адрес сервера (иначе SELFSYNC_PUBLIC_URL).
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        name: String,
    },
    /// Загрузить существующую папку (открытым текстом).
    Import {
        #[arg(long)]
        vault: String,
        #[arg(long)]
        from: PathBuf,
    },
    /// Сборка мусора. По умолчанию только показывает план.
    Gc {
        #[arg(long)]
        vault: Option<String>,
        /// Только показать план (поведение по умолчанию).
        #[arg(long)]
        dry_run: bool,
        /// Выполнить удаление.
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
        /// Сколько последних ревизий файла хранить всегда.
        #[arg(long, default_value_t = 20)]
        keep_revisions: u64,
        /// Ревизии моложе стольких дней не удаляются.
        #[arg(long, default_value_t = 30)]
        keep_days: u64,
    },
    /// Стереть удалённые файлы с истёкшим окном хранения (для systemd-таймера).
    Sweep,
    /// Консистентная копия данных в каталог.
    Backup { dir: PathBuf },
    /// Проверка живости (код 0/1), для HEALTHCHECK.
    Healthcheck {
        #[arg(
            long,
            env = "SELFSYNC_HEALTHCHECK_URL",
            default_value = "http://127.0.0.1:8085/v1/health"
        )]
        url: String,
    },
}

#[derive(Subcommand)]
enum VaultCmd {
    /// Список vault'ов.
    List,
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Выдать токен новому устройству (vault создаётся при первом использовании).
    Add {
        #[arg(long)]
        vault: String,
        #[arg(long)]
        name: String,
    },
    /// Список устройств.
    List {
        #[arg(long)]
        vault: Option<String>,
    },
    /// Отозвать устройство (без --vault — во всех vault'ах).
    Revoke {
        #[arg(long)]
        name: String,
        #[arg(long)]
        vault: Option<String>,
    },
}

fn idle(flag: Option<String>, config: &Config, default: Duration) -> anyhow::Result<Duration> {
    match flag {
        Some(s) => parse_duration(&s),
        None => Ok(config.idle_timeout.unwrap_or(default)),
    }
}

fn run() -> anyhow::Result<ExitCode> {
    // fcgiwrap запускает бинарь без аргументов: CGI определяется по окружению.
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 && cgi::detected() {
        let config = Config::load(None, None)?;
        init_logging(&config.log);
        cgi::run(AppState::new(config, Mode::Cgi))?;
        return Ok(ExitCode::SUCCESS);
    }
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref(), cli.data.clone())?;
    let Some(command) = cli.cmd else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        return Ok(ExitCode::from(2));
    };
    init_logging(&config.log);
    match command {
        Cmd::Cgi => cgi::run(AppState::new(config, Mode::Cgi))?,
        Cmd::Serve {
            listen,
            idle_timeout,
        } => {
            let idle = idle(idle_timeout, &config, Duration::ZERO)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let l = modes::bind(&listen).await?;
                modes::run(AppState::new(config, Mode::Serve), l, idle).await
            })?;
        }
        Cmd::Socket { idle_timeout } => {
            let idle = idle(idle_timeout, &config, Duration::from_secs(600))?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let l = modes::from_systemd()?;
                modes::run(AppState::new(config, Mode::Socket), l, idle).await
            })?;
        }
        Cmd::Migrate { vault } => cmd::migrate(&config, vault.as_deref())?,
        Cmd::Vault(VaultCmd::List) => cmd::vault_list(&config)?,
        Cmd::Token(TokenCmd::Add { vault, name }) => cmd::token_add(&config, &vault, &name)?,
        Cmd::Token(TokenCmd::List { vault }) => cmd::token_list(&config, vault.as_deref())?,
        Cmd::Token(TokenCmd::Revoke { name, vault }) => {
            cmd::token_revoke(&config, vault.as_deref(), &name)?
        }
        Cmd::Link { vault, url, name } => cmd::link(&config, &vault, url.as_deref(), &name)?,
        Cmd::Import { vault, from } => {
            let r = cmd::import(&config, &vault, &from, config.max_blob_size)
                .with_context(|| format!("импорт из {}", from.display()))?;
            println!(
                "импортировано файлов: {}, без изменений: {}, папок: {}",
                r.files, r.unchanged, r.folders
            );
            for (p, why) in &r.skipped {
                println!("пропущено: {p} — {why}");
            }
        }
        Cmd::Gc {
            vault,
            dry_run: _,
            yes,
            keep_revisions,
            keep_days,
        } => cmd::gc(
            &config,
            vault.as_deref(),
            GcOptions {
                keep_revisions,
                keep_days,
            },
            yes,
        )?,
        Cmd::Sweep => cmd::sweep(&config)?,
        Cmd::Backup { dir } => cmd::backup(&config, &dir)?,
        Cmd::Healthcheck { url } => {
            return Ok(if cmd::healthcheck(&url) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ошибка: {e:#}");
            ExitCode::FAILURE
        }
    }
}
