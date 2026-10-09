use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use selfsync_cli::config::{Config, STATE_DIR};
use selfsync_cli::runner::{self, Options, Outcome};
use selfsync_core::engine::{UiCommand, UiResult};
use selfsync_core::index::Index;

#[derive(Parser)]
#[command(
    name = "selfsync-cli",
    version,
    about = "Синхронизация папки с сервером selfsync"
)]
struct Cli {
    /// Синхронизируемая папка.
    #[arg(long, global = true, default_value = ".")]
    dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Подключить папку токеном (`selfsync token add` на сервере).
    Init {
        #[arg(long)]
        server: String,
        #[arg(long, env = "SELFSYNC_TOKEN")]
        token: String,
        /// Имя устройства (по умолчанию — имя машины).
        #[arg(long)]
        name: Option<String>,
        /// Сервер работает постоянно (socket/serve): long-poll вместо опроса.
        #[arg(long)]
        use_wait: bool,
    },
    /// Подключить папку одноразовым кодом (`selfsync link` или «Подключить новое устройство»).
    Connect {
        #[arg(long)]
        server: String,
        #[arg(long)]
        code: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        use_wait: bool,
    },
    /// Синхронизировать: следить за папкой до Ctrl-C (или один цикл с --once).
    Run {
        #[arg(long)]
        once: bool,
        /// Файл с паролем шифрования (или переменная SELFSYNC_PASSWORD).
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Не запоминать ключ в .selfsync/key.
        #[arg(long)]
        no_remember: bool,
    },
    /// Включить сквозное шифрование vault'а (пароль понадобится всем устройствам).
    Encrypt {
        /// Файл с паролем (или переменная SELFSYNC_PASSWORD).
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Состояние: подключение, очередь, конфликты.
    Status,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SELFSYNC_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(real_main(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ошибка: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main(cli: Cli) -> anyhow::Result<ExitCode> {
    let dir = cli.dir;
    match cli.cmd {
        Cmd::Init {
            server,
            token,
            name,
            use_wait,
        } => {
            std::fs::create_dir_all(&dir).with_context(|| format!("папка {}", dir.display()))?;
            let mut cfg = Config::load(&dir).unwrap_or_default();
            cfg.server = server.trim_end_matches('/').to_owned();
            cfg.token = token;
            cfg.use_wait = use_wait;
            if let Some(n) = name {
                cfg.device_name = n;
            }
            cfg.save(&dir)?;
            println!(
                "папка {} подключена к {}; синк: selfsync-cli run --dir {}",
                dir.display(),
                cfg.server,
                dir.display()
            );
        }
        Cmd::Connect {
            server,
            code,
            name,
            use_wait,
        } => {
            std::fs::create_dir_all(dir.join(STATE_DIR))?;
            let mut cfg = Config::load(&dir).unwrap_or_default();
            if let Some(n) = name {
                cfg.device_name = n;
            }
            let server = server.trim_end_matches('/').to_owned();
            let r = runner::command(
                &dir,
                &server,
                None,
                &cfg.device_name,
                UiCommand::Redeem {
                    code,
                    name: cfg.device_name.clone(),
                },
            )
            .await?;
            match r {
                UiResult::Token {
                    token,
                    vault,
                    device_name,
                    ..
                } => {
                    cfg.server = server;
                    cfg.token = token;
                    cfg.vault = vault.clone();
                    cfg.device_name = device_name;
                    cfg.use_wait = use_wait;
                    cfg.save(&dir)?;
                    println!("подключено к vault «{vault}» как «{}»", cfg.device_name);
                }
                UiResult::Error { message, .. } => anyhow::bail!("сервер отказал: {message}"),
                other => anyhow::bail!("неожиданный ответ: {other:?}"),
            }
        }
        Cmd::Run {
            once,
            password_file,
            no_remember,
        } => {
            let cfg = Config::load(&dir)?;
            let opts = Options {
                once,
                watch: true,
                password: read_password(password_file)?,
                remember_key: !no_remember,
                enable_encryption: false,
            };
            return Ok(outcome(runner::run(&dir, &cfg, opts).await?));
        }
        Cmd::Encrypt { password_file } => {
            let cfg = Config::load(&dir)?;
            let password = read_password(password_file)?
                .context("нужен пароль: --password-file или SELFSYNC_PASSWORD")?;
            let opts = Options {
                once: true,
                watch: false,
                password: Some(password),
                remember_key: true,
                enable_encryption: true,
            };
            let code = outcome(runner::run(&dir, &cfg, opts).await?);
            if code == ExitCode::SUCCESS {
                println!("шифрование включено; на других устройствах понадобится этот пароль");
            }
            return Ok(code);
        }
        Cmd::Status => {
            let cfg = Config::load(&dir)?;
            println!("сервер:     {}", cfg.server);
            println!(
                "vault:      {}",
                if cfg.vault.is_empty() {
                    "?"
                } else {
                    &cfg.vault
                }
            );
            println!("устройство: {}", cfg.device_name);
            match std::fs::read(dir.join(STATE_DIR).join("index.bin"))
                .ok()
                .map(|b| Index::decode(&b))
            {
                Some(Ok(idx)) => {
                    let files = idx
                        .files
                        .values()
                        .filter(|f| !f.folder && f.local.is_some())
                        .count();
                    println!("файлов:     {files}");
                    println!("к отправке: {}", idx.pending_count());
                    println!(
                        "шифрование: {}",
                        if matches!(idx.mode, selfsync_core::index::VaultMode::Plain) {
                            "нет"
                        } else {
                            "да"
                        }
                    );
                    println!("курсор:     {}", idx.last_seq);
                    for c in &idx.conflicts {
                        println!("конфликт:   {} → копия {}", c.path, c.copy);
                    }
                }
                Some(Err(e)) => println!("индекс:     повреждён ({e})"),
                None => println!("индекс:     ещё не синхронизировалось"),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn read_password(file: Option<PathBuf>) -> anyhow::Result<Option<String>> {
    Ok(match file {
        Some(p) => Some(
            std::fs::read_to_string(&p)
                .with_context(|| format!("пароль из {}", p.display()))?
                .trim_end_matches(['\n', '\r'])
                .to_owned(),
        ),
        None => std::env::var("SELFSYNC_PASSWORD").ok(),
    })
}

fn outcome(o: Outcome) -> ExitCode {
    match o {
        Outcome::Synced | Outcome::Interrupted => ExitCode::SUCCESS,
        Outcome::Failed(r) => {
            eprintln!("синк не удался: {r}");
            ExitCode::from(1)
        }
        Outcome::NeedPassword => {
            eprintln!("vault зашифрован: нужен пароль (--password-file или SELFSYNC_PASSWORD)");
            ExitCode::from(2)
        }
        Outcome::Blocked(r) => {
            eprintln!("синк остановлен: {r}");
            ExitCode::from(3)
        }
    }
}
