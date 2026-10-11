use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use selfsync_cli::config::{Config, STATE_DIR};
use selfsync_cli::runner::{self, Options, Outcome, VaultCommand};
use selfsync_core::engine::{ConflictChoice, UiCommand, UiResult};
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
        /// Команда оболочки после каждого цикла, который что-то изменил (в папке, с
        /// SELFSYNC_DIR, SELFSYNC_PULLED, SELFSYNC_PUSHED, SELFSYNC_CHANGES_FILE).
        /// По умолчанию — `exec` из .selfsync/config.toml.
        #[arg(long)]
        exec: Option<String>,
        /// Хук дольше этого останавливается (по умолчанию 10m).
        #[arg(long)]
        exec_timeout: Option<String>,
    },
    /// Включить сквозное шифрование vault'а (пароль понадобится всем устройствам).
    Encrypt {
        /// Файл с паролем (или переменная SELFSYNC_PASSWORD).
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Состояние: подключение, очередь, конфликты.
    Status,
    /// История файла: ревизии на сервере (номер, дата, размер, устройство).
    History {
        /// Путь внутри папки.
        path: String,
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Корзина сервера: удалённые файлы и когда они сотрутся окончательно.
    Trash {
        /// Показать и те, чей срок уже вышел (сотрутся при ближайшем sweep).
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Вернуть файлы из корзины сервера.
    Restore {
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Стереть файлы из корзины сервера окончательно (вместе с историей).
    Purge {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Подтверждение: без него ничего не стирается.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Откатить файл к ревизии (неотправленная текущая версия остаётся копией рядом).
    Revert {
        path: String,
        #[arg(long)]
        rev: u64,
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Нерешённые конфликты.
    Conflicts {
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Решить конфликт: оставить обе версии, свою или серверную.
    Resolve {
        id: u64,
        #[arg(long, value_enum)]
        keep: Keep,
        #[command(flatten)]
        v: VaultArgs,
    },
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
        Cmd::History { path, v } => {
            let path = vault_path(&dir, &path);
            let cmd = VaultCommand::Ui(UiCommand::History { path: path.clone() });
            return vault_command(&dir, v, cmd, |r, json| {
                let UiResult::History { revisions } = r else {
                    anyhow::bail!("неожиданный ответ: {r:?}");
                };
                if json {
                    return print_json(&revisions);
                }
                if revisions.is_empty() {
                    println!("у {path} нет истории на сервере");
                }
                for x in revisions {
                    let what = if x.deleted {
                        "удалён".to_owned()
                    } else {
                        format!("{} байт", x.size)
                    };
                    let from = x
                        .renamed_from
                        .map(|f| format!(" (из {f})"))
                        .unwrap_or_default();
                    println!(
                        "{}\t{}\t{what}\t#{}{from}",
                        x.rev,
                        fmt_time(x.mtime),
                        x.device
                    );
                }
                Ok(())
            })
            .await;
        }
        Cmd::Trash { all, v } => {
            let cmd = VaultCommand::Ui(UiCommand::ListDeleted);
            let now = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis()),
            )
            .unwrap_or(i64::MAX);
            return vault_command(&dir, v, cmd, |r, json| {
                let UiResult::Deleted { items } = r else {
                    anyhow::bail!("неожиданный ответ: {r:?}");
                };
                let items: Vec<_> = items
                    .into_iter()
                    .filter(|x| all || x.expires_at > now)
                    .collect();
                if json {
                    return print_json(&items);
                }
                if items.is_empty() {
                    println!("корзина пуста");
                }
                for x in items {
                    println!(
                        "{}\tудалён {}\tсотрётся {}\t{} байт",
                        x.path,
                        fmt_time(x.deleted_at),
                        fmt_time(x.expires_at),
                        x.size
                    );
                }
                Ok(())
            })
            .await;
        }
        Cmd::Restore { paths, v } => {
            let paths = paths.iter().map(|p| vault_path(&dir, p)).collect();
            let cmd = VaultCommand::Ui(UiCommand::RestoreDeleted { paths });
            return vault_command(&dir, v, cmd, |r, json| {
                let UiResult::Restored { count } = r else {
                    anyhow::bail!("неожиданный ответ: {r:?}");
                };
                if json {
                    return print_json(&serde_json::json!({ "count": count }));
                }
                println!("восстановлено: {count}");
                Ok(())
            })
            .await;
        }
        Cmd::Purge { paths, yes, v } => {
            if !yes {
                anyhow::bail!("стирание из корзины необратимо: добавьте --yes");
            }
            let paths = paths.iter().map(|p| vault_path(&dir, p)).collect();
            let cmd = VaultCommand::Ui(UiCommand::PurgeDeleted { paths });
            return vault_command(&dir, v, cmd, |r, json| {
                let UiResult::Restored { count } = r else {
                    anyhow::bail!("неожиданный ответ: {r:?}");
                };
                if json {
                    return print_json(&serde_json::json!({ "count": count }));
                }
                println!("стёрто окончательно: {count}");
                Ok(())
            })
            .await;
        }
        Cmd::Revert { path, rev, v } => {
            let path = vault_path(&dir, &path);
            let cmd = VaultCommand::Ui(UiCommand::RestoreRevision {
                path: path.clone(),
                rev,
            });
            return vault_command(&dir, v, cmd, |_, json| {
                if json {
                    return print_json(&serde_json::json!({ "ok": true }));
                }
                println!("{path}: ревизия {rev} восстановлена");
                Ok(())
            })
            .await;
        }
        Cmd::Conflicts { v } => {
            let cmd = VaultCommand::Ui(UiCommand::Conflicts);
            return vault_command(&dir, v, cmd, |r, json| {
                let UiResult::Conflicts { items } = r else {
                    anyhow::bail!("неожиданный ответ: {r:?}");
                };
                if json {
                    return print_json(&items);
                }
                if items.is_empty() {
                    println!("конфликтов нет");
                }
                for c in items {
                    println!("{}\t{}\tкопия {}\t{}", c.id, c.path, c.copy, fmt_time(c.at));
                }
                Ok(())
            })
            .await;
        }
        Cmd::Resolve { id, keep, v } => {
            let choice = match keep {
                Keep::Both => ConflictChoice::KeepBoth,
                Keep::Mine => ConflictChoice::KeepMine,
                Keep::Server => ConflictChoice::KeepServer,
            };
            // Неизвестный id ядро молча пропускает — проверить заранее по индексу.
            let known = std::fs::read(dir.join(STATE_DIR).join("index.bin"))
                .ok()
                .and_then(|b| Index::decode(&b).ok())
                .is_some_and(|idx| idx.conflicts.iter().any(|c| c.id == id));
            if !known {
                anyhow::bail!("конфликта {id} нет: список — selfsync-cli conflicts");
            }
            let cmd = VaultCommand::Resolve { id, choice };
            return vault_command(&dir, v, cmd, |_, json| {
                if json {
                    return print_json(&serde_json::json!({ "ok": true }));
                }
                println!("конфликт {id} решён");
                Ok(())
            })
            .await;
        }
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
                UiResult::Error {
                    code, retry_after, ..
                } if code == "rate_limited" => anyhow::bail!(
                    "слишком много неверных кодов с этого адреса, подождите {retry_after} с"
                ),
                UiResult::Error { message, .. } => anyhow::bail!("сервер отказал: {message}"),
                other => anyhow::bail!("неожиданный ответ: {other:?}"),
            }
        }
        Cmd::Run {
            once,
            password_file,
            no_remember,
            exec,
            exec_timeout,
        } => {
            let cfg = Config::load(&dir)?;
            let hook = match exec.or_else(|| cfg.exec.clone()) {
                Some(command) if !command.trim().is_empty() => {
                    let timeout = match exec_timeout.or_else(|| cfg.exec_timeout.clone()) {
                        Some(t) => humantime::parse_duration(t.trim())
                            .with_context(|| format!("неверный таймаут хука: {t}"))?,
                        None => selfsync_cli::hook::DEFAULT_TIMEOUT,
                    };
                    Some(selfsync_cli::hook::HookConfig { command, timeout })
                }
                _ => None,
            };
            let opts = Options {
                once,
                watch: true,
                password: read_password(password_file)?,
                remember_key: !no_remember,
                enable_encryption: false,
                hook,
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
                hook: None,
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
            if dir.join(".git").exists()
                && !selfsync_core::exclude::Excludes::new(&cfg.excludes).is_excluded(".git/HEAD")
            {
                println!(
                    "внимание:   .git не исключён — репозиторий уйдёт на все устройства; \
                     добавьте \".git/\" в excludes в {}",
                    Config::path(&dir).display()
                );
            }
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
                    if dir.join(STATE_DIR).join(runner::READ_ONLY_MARKER).exists() {
                        println!("режим:      только чтение (локальные правки не отправляются)");
                    }
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

/// Общее у команд корзины и истории.
#[derive(Args)]
struct VaultArgs {
    /// Ответ в JSON (для скриптов).
    #[arg(long)]
    json: bool,
    /// Файл с паролем шифрования (или переменная SELFSYNC_PASSWORD).
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// Не запоминать ключ в .selfsync/key.
    #[arg(long)]
    no_remember: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Keep {
    Both,
    Mine,
    Server,
}

/// Путь внутри папки: относительный как есть, `./` и абсолютный внутри папки —
/// приводятся к пути vault'а.
fn vault_path(dir: &std::path::Path, p: &str) -> String {
    let path = std::path::Path::new(p);
    let rel = if path.is_absolute() {
        let base = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        let full = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        full.strip_prefix(&base)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_owned())
    } else {
        p.to_owned()
    };
    let rel = rel.replace('\\', "/");
    rel.trim_start_matches("./").trim_matches('/').to_owned()
}

fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_owned();
    }
    let t =
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0));
    humantime::format_rfc3339_seconds(t).to_string()
}

fn print_json<T: serde::Serialize>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Команда на движке папки: печать ответа и код выхода как у `run --once`.
async fn vault_command(
    dir: &std::path::Path,
    v: VaultArgs,
    cmd: VaultCommand,
    print: impl FnOnce(UiResult, bool) -> anyhow::Result<()>,
) -> anyhow::Result<ExitCode> {
    let cfg = Config::load(dir)?;
    let (o, r) = runner::with_vault(
        dir,
        &cfg,
        read_password(v.password_file)?,
        !v.no_remember,
        cmd,
    )
    .await?;
    match r {
        Some(UiResult::Error { code, message, .. }) => {
            if v.json {
                print_json(&serde_json::json!({ "error": code, "message": message }))?;
            }
            eprintln!("ошибка: {message}");
            Ok(ExitCode::from(1))
        }
        Some(r) => {
            print(r, v.json)?;
            Ok(outcome(o))
        }
        None => Ok(outcome(o)),
    }
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
