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
        /// Создать и фразу восстановления (24 слова в stdout, один раз). Запись ключа
        /// станет формата 2: плагин и CLI 0.1.x её не откроют — обновите все устройства.
        #[arg(long)]
        recovery: bool,
    },
    /// Фраза восстановления зашифрованного vault'а.
    #[command(subcommand)]
    Recovery(RecoveryCmd),
    /// Забыли пароль: раскрыть vault фразой восстановления и задать новый пароль.
    Recover {
        /// Файл с 24 словами (иначе — ввод с терминала).
        #[arg(long)]
        phrase_file: Option<PathBuf>,
        /// Файл с новым паролем (иначе — ввод с терминала).
        #[arg(long)]
        new_password_file: Option<PathBuf>,
        /// Не запоминать ключ в .selfsync/key.
        #[arg(long)]
        no_remember: bool,
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
    /// Файлы, не скачанные из-за лимита `max_download_size`.
    Skipped {
        #[command(flatten)]
        v: VaultArgs,
    },
    /// Скачать пропущенный файл несмотря на лимит (дальше он синхронизируется).
    Download {
        path: String,
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

/// Первая выпущенная версия, которая открывает запись ключа формата 2 (с фразой).
const RECOVERY_MIN: &str = "0.2.1";

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
        Cmd::Skipped { v } => {
            let cmd = VaultCommand::Ui(UiCommand::Skipped);
            return vault_command(&dir, v, cmd, |r, json| {
                let UiResult::Skipped { files } = r else {
                    anyhow::bail!("неожиданный ответ: {r:?}");
                };
                if json {
                    return print_json(&files);
                }
                if files.is_empty() {
                    println!("пропущенных нет");
                }
                for f in files {
                    println!("{}\t{}", f.path, f.size);
                }
                Ok(())
            })
            .await;
        }
        Cmd::Download { path, v } => {
            let path = vault_path(&dir, &path);
            let cmd = VaultCommand::Ui(UiCommand::Download { path: path.clone() });
            // Команда ставит файл в очередь; скачивает его следующий цикл синка.
            let json = v.json;
            let code = vault_command(&dir, v, cmd, |_, _| Ok(())).await?;
            if code != ExitCode::SUCCESS {
                return Ok(code);
            }
            let ok = dir.join(&path).is_file();
            if json {
                print_json(&serde_json::json!({ "path": path, "downloaded": ok }))?;
            } else if ok {
                println!("скачан: {path}");
            } else {
                eprintln!("{path}: не скачался — подробности в журнале, повторите позже");
            }
            return Ok(if ok { code } else { ExitCode::from(1) });
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
                recovery: false,
            };
            return Ok(outcome(runner::run(&dir, &cfg, opts).await?));
        }
        Cmd::Recovery(rc) => {
            let (password_file, create) = match rc {
                RecoveryCmd::Create { password_file } => (password_file, true),
                RecoveryCmd::Remove { password_file } => (password_file, false),
            };
            let password = read_password(password_file)?
                .context("нужен пароль: --password-file или SELFSYNC_PASSWORD")?;
            let cmd = if create {
                UiCommand::CreateRecovery {
                    password: password.clone(),
                }
            } else {
                UiCommand::RemoveRecovery {
                    password: password.clone(),
                }
            };
            // Пароль нужен и синку перед командой, если ключ ещё не запомнен.
            let cfg = Config::load(&dir)?;
            if create {
                warn_old_devices(&dir, &cfg, RECOVERY_MIN, "запись ключа с фразой").await;
            }
            let (o, r) =
                runner::with_vault(&dir, &cfg, Some(password), true, VaultCommand::Ui(cmd)).await?;
            return match r {
                Some(UiResult::RecoveryPhrase { words }) => {
                    runner::print_phrase(&words);
                    eprintln!(
                        "Запись ключа теперь формата 2: устройствам нужен плагин или CLI с \
                         поддержкой фразы восстановления."
                    );
                    Ok(outcome(o))
                }
                Some(UiResult::Ok) => {
                    println!("фраза восстановления удалена");
                    Ok(outcome(o))
                }
                Some(UiResult::Error { message, .. }) => {
                    eprintln!("ошибка: {message}");
                    Ok(ExitCode::from(1))
                }
                Some(other) => anyhow::bail!("неожиданный ответ: {other:?}"),
                None => Ok(outcome(o)),
            };
        }
        Cmd::Recover {
            phrase_file,
            new_password_file,
            no_remember,
        } => {
            let cfg = Config::load(&dir)?;
            let phrase = read_secret(phrase_file, "фраза восстановления (24 слова)")?;
            // Проверка до сети: номер неверного слова сразу.
            if let Err(e) = selfsync_core::crypto::RecoveryEntropy::from_phrase(&phrase) {
                anyhow::bail!("фраза: {e}");
            }
            let new_password = read_secret(new_password_file, "новый пароль")?;
            anyhow::ensure!(!new_password.is_empty(), "пустой пароль");
            let r = runner::folder_command(
                &dir,
                &cfg,
                UiCommand::Recover {
                    phrase,
                    new_password: new_password.clone(),
                    remember: !no_remember,
                },
            )
            .await?;
            if let UiResult::Error { message, .. } = r {
                eprintln!("ошибка: {message}");
                return Ok(ExitCode::from(1));
            }
            eprintln!("доступ восстановлен, пароль заменён; синхронизация…");
            let opts = Options {
                once: true,
                watch: false,
                password: Some(new_password),
                remember_key: !no_remember,
                enable_encryption: false,
                hook: None,
                recovery: false,
            };
            return Ok(outcome(runner::run(&dir, &cfg, opts).await?));
        }
        Cmd::Encrypt {
            password_file,
            recovery,
        } => {
            let cfg = Config::load(&dir)?;
            let password = read_password(password_file)?
                .context("нужен пароль: --password-file или SELFSYNC_PASSWORD")?;
            if recovery {
                warn_old_devices(&dir, &cfg, RECOVERY_MIN, "запись ключа с фразой").await;
            }
            let opts = Options {
                once: true,
                watch: false,
                password: Some(password),
                remember_key: true,
                enable_encryption: true,
                hook: None,
                recovery,
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

#[derive(Subcommand)]
enum RecoveryCmd {
    /// Создать фразу (или новую вместо прежней: та перестанет работать).
    Create {
        /// Файл с паролем (или переменная SELFSYNC_PASSWORD).
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Удалить фразу восстановления.
    Remove {
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
}

/// Секрет из файла или с терминала (без эха).
fn read_secret(file: Option<PathBuf>, prompt: &str) -> anyhow::Result<String> {
    if let Some(p) = file {
        return Ok(std::fs::read_to_string(&p)
            .with_context(|| format!("{}", p.display()))?
            .trim()
            .to_owned());
    }
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    anyhow::ensure!(
        stdin.is_terminal(),
        "нет терминала: передайте файл ({prompt})"
    );
    eprint!("{prompt}: ");
    std::io::stderr().flush()?;
    let _echo = EchoOff::new();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    eprintln!();
    Ok(line.trim().to_owned())
}

/// Выключенное эхо терминала на время ввода секрета.
struct EchoOff(#[cfg(unix)] Option<libc::termios>);

impl EchoOff {
    fn new() -> EchoOff {
        #[cfg(unix)]
        {
            // SAFETY: tcgetattr/tcsetattr над stdin; структура заполняется ОС.
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) == 0 {
                    let old = t;
                    t.c_lflag &= !libc::ECHO;
                    libc::tcsetattr(0, libc::TCSANOW, &t);
                    return EchoOff(Some(old));
                }
            }
            EchoOff(None)
        }
        #[cfg(not(unix))]
        EchoOff()
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(t) = self.0 {
            // SAFETY: восстановление сохранённых атрибутов терминала.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
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

/// Предупредить об устройствах, которые старее `min` (или версия которых
/// неизвестна): они не поймут то, что сейчас будет сделано. Без сети — молча.
async fn warn_old_devices(dir: &std::path::Path, cfg: &Config, min: &str, what: &str) {
    let Ok(UiResult::Devices { devices, .. }) =
        runner::folder_command(dir, cfg, UiCommand::Devices).await
    else {
        return;
    };
    let old: Vec<String> = devices
        .iter()
        .filter(|d| !d.revoked && !d.current)
        .filter(|d| selfsync_core::version::older_than(&d.client, min))
        .map(|d| {
            if d.client.is_empty() {
                format!("«{}» (версия неизвестна)", d.name)
            } else {
                format!("«{}» ({})", d.name, d.client)
            }
        })
        .collect();
    if !old.is_empty() {
        eprintln!(
            "внимание: {what} не поймут устройства старее {min}: {}. Обновите на них \
             плагин или selfsync-cli.",
            old.join(", ")
        );
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
