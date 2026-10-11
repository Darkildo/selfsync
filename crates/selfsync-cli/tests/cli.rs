//! Клиент против настоящего сервера (в том же процессе): две папки
//! синхронизируются циклами `run --once`.

use std::path::Path;
use std::time::Duration;

use selfsync_cli::config::Config;
use selfsync_cli::runner::{self, Options, Outcome};
use selfsync_core::engine::{UiCommand, UiResult};
use selfsync_server::{AppState, Mode};

struct Server {
    url: String,
    state: AppState,
    _dir: tempfile::TempDir,
}

async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        selfsync_server::Config::for_tests(dir.path().to_path_buf()),
        Mode::Serve,
    );
    let listener = selfsync_server::modes::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.describe());
    let st = state.clone();
    tokio::spawn(async move { selfsync_server::modes::run(st, listener, Duration::ZERO).await });
    Server {
        url,
        state,
        _dir: dir,
    }
}

impl Server {
    fn token(&self, vault: &str, name: &str) -> String {
        let c = self.state.server_pool().unwrap().get().unwrap();
        selfsync_server::db::server::add_device(&c, vault, name, false)
            .unwrap()
            .1
    }

    fn join_code(&self, vault: &str, name: &str) -> String {
        let c = self.state.server_pool().unwrap().get().unwrap();
        selfsync_server::db::server::create_join_code(&c, vault, name, None, false)
            .unwrap()
            .0
    }
}

fn folder(srv: &Server, vault: &str, name: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        server: srv.url.clone(),
        token: srv.token(vault, name),
        device_name: name.into(),
        ..Config::default()
    };
    cfg.save(dir.path()).unwrap();
    (dir, cfg)
}

fn once(password: Option<&str>) -> Options {
    Options {
        once: true,
        watch: false,
        password: password.map(str::to_owned),
        remember_key: true,
        enable_encryption: false,
        hook: None,
    }
}

async fn sync(dir: &Path, cfg: &Config) -> Outcome {
    runner::run(dir, cfg, once(None)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_folders_sync_edit_delete() {
    let srv = server().await;
    let (a, ca) = folder(&srv, "cli", "a");
    let (b, cb) = folder(&srv, "cli", "b");
    std::fs::create_dir_all(a.path().join("notes")).unwrap();
    std::fs::write(a.path().join("notes/one.md"), "one\n").unwrap();
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
    assert_eq!(
        std::fs::read_to_string(b.path().join("notes/one.md")).unwrap(),
        "one\n"
    );

    std::fs::write(b.path().join("notes/one.md"), "one\ntwo\n").unwrap();
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    assert_eq!(
        std::fs::read_to_string(a.path().join("notes/one.md")).unwrap(),
        "one\ntwo\n"
    );

    std::fs::remove_file(a.path().join("notes/one.md")).unwrap();
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
    assert!(!b.path().join("notes/one.md").exists());
    let trash: Vec<_> = std::fs::read_dir(b.path().join(".trash"))
        .unwrap()
        .collect();
    assert_eq!(trash.len(), 1, "удалённое — в корзину, а не стёрто");
    // Служебный каталог не синхронизируется: у b осталась своя конфигурация.
    assert_eq!(Config::load(b.path()).unwrap().token, cb.token);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_by_code() {
    let srv = server().await;
    let (a, ca) = folder(&srv, "join", "a");
    std::fs::write(a.path().join("s.md"), "hello\n").unwrap();
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);

    let fresh = tempfile::tempdir().unwrap();
    let code = srv.join_code("join", "laptop");
    let r = runner::command(
        fresh.path(),
        &srv.url,
        None,
        "laptop",
        UiCommand::Redeem {
            code,
            name: "laptop".into(),
        },
    )
    .await
    .unwrap();
    let UiResult::Token { token, vault, .. } = r else {
        panic!("{r:?}")
    };
    assert_eq!(vault, "join");
    let cfg = Config {
        server: srv.url.clone(),
        token,
        vault,
        device_name: "laptop".into(),
        ..Config::default()
    };
    cfg.save(fresh.path()).unwrap();
    assert_eq!(sync(fresh.path(), &cfg).await, Outcome::Synced);
    assert_eq!(
        std::fs::read_to_string(fresh.path().join("s.md")).unwrap(),
        "hello\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypt_then_password_on_other_device() {
    const PW: &str = "correct horse battery staple";
    let srv = server().await;
    let (a, ca) = folder(&srv, "enc", "a");
    let (b, cb) = folder(&srv, "enc", "b");
    std::fs::write(a.path().join("s.md"), "secret\n").unwrap();
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);

    let enable = Options {
        enable_encryption: true,
        hook: None,
        ..once(Some(PW))
    };
    assert_eq!(
        runner::run(a.path(), &ca, enable).await.unwrap(),
        Outcome::Synced
    );
    assert!(
        a.path().join(".selfsync/key").exists(),
        "ключ запомнен для перезапусков"
    );

    std::fs::write(b.path().join("b.md"), "from b\n").unwrap();
    assert_eq!(sync(b.path(), &cb).await, Outcome::NeedPassword);
    assert_eq!(
        runner::run(b.path(), &cb, once(Some(PW))).await.unwrap(),
        Outcome::Synced
    );
    // Перезапуск без пароля: ключ уже на устройстве.
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    assert_eq!(
        std::fs::read_to_string(a.path().join("b.md")).unwrap(),
        "from b\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_token_blocks() {
    let srv = server().await;
    let (a, ca) = folder(&srv, "rv", "gone");
    {
        let c = srv.state.server_pool().unwrap().get().unwrap();
        selfsync_server::db::server::revoke_by_name(&c, Some("rv"), "gone").unwrap();
    }
    std::fs::write(a.path().join("x.md"), "x\n").unwrap();
    assert_eq!(
        sync(a.path(), &ca).await,
        Outcome::Blocked("unauthorized".into())
    );
}

/// Демон следит за папкой: новый файл уходит на сервер без ручного запуска.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_picks_up_changes() {
    let srv = server().await;
    let (a, mut ca) = folder(&srv, "watch", "a");
    let (b, cb) = folder(&srv, "watch", "b");
    ca.debounce_ms = 200;
    let root = a.path().to_path_buf();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let cfg = ca.clone();
            let daemon = tokio::task::spawn_local(async move {
                let opts = Options {
                    once: false,
                    watch: true,
                    ..once(None)
                };
                runner::run(&root, &cfg, opts).await
            });
            tokio::time::sleep(Duration::from_millis(500)).await;
            std::fs::write(a.path().join("watched.md"), "seen\n").unwrap();
            let mut got = None;
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
                got = std::fs::read_to_string(b.path().join("watched.md")).ok();
                if got.is_some() {
                    break;
                }
            }
            daemon.abort();
            assert_eq!(got.as_deref(), Some("seen\n"));
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_folder_receives_and_keeps_local_edits() {
    let srv = server().await;
    let (a, ca) = folder(&srv, "ro", "a");
    let b = tempfile::tempdir().unwrap();
    let cb = Config {
        server: srv.url.clone(),
        token: {
            let c = srv.state.server_pool().unwrap().get().unwrap();
            selfsync_server::db::server::add_device(&c, "ro", "mirror", true)
                .unwrap()
                .1
        },
        device_name: "mirror".into(),
        ..Config::default()
    };
    cb.save(b.path()).unwrap();
    std::fs::write(a.path().join("note.md"), "from a\n").unwrap();
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    // свои правки зеркала не уходят, но синк считается удавшимся (код 0)
    std::fs::write(b.path().join("local.md"), "only here\n").unwrap();
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
    assert_eq!(
        std::fs::read_to_string(b.path().join("note.md")).unwrap(),
        "from a\n"
    );
    assert!(b.path().join(".selfsync/read-only").exists());
    assert!(!a.path().join(".selfsync/read-only").exists());
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    assert!(!a.path().join("local.md").exists());
    // повторный запуск — тот же исход, файл на месте
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
    assert!(b.path().join("local.md").exists());
    // создать ссылку подключения с зеркала нельзя
    let r = runner::command(
        b.path(),
        &srv.url,
        Some(cb.token.clone()),
        "mirror",
        UiCommand::CreateJoin {
            name: "x".into(),
            read_only: false,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(&r, UiResult::Error { code, .. } if code == "read_only"),
        "{r:?}"
    );
}

fn with_hook(command: String, timeout: Duration) -> Options {
    Options {
        hook: Some(selfsync_cli::hook::HookConfig { command, timeout }),
        ..once(None)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_runs_after_changes_with_env() {
    let srv = server().await;
    let (a, ca) = folder(&srv, "hook", "a");
    let (b, cb) = folder(&srv, "hook", "b");
    let out = tempfile::tempdir().unwrap();
    let marker = out.path().join("marker");
    let cmd = format!(
        "printf '%s %s %s\\n' \"$SELFSYNC_PULLED\" \"$SELFSYNC_PUSHED\" \"$(pwd)\" > '{m}'; \
         cat \"$SELFSYNC_CHANGES_FILE\" >> '{m}'; test \"$SELFSYNC_DIR\" = \"$(pwd)\"",
        m = marker.display()
    );
    assert_eq!(sync(b.path(), &cb).await, Outcome::Synced);
    std::fs::create_dir_all(a.path().join("dir")).unwrap();
    std::fs::write(a.path().join("dir/n.md"), "x\n").unwrap();
    std::fs::write(a.path().join("top.md"), "y\n").unwrap();
    assert_eq!(sync(a.path(), &ca).await, Outcome::Synced);
    let r = runner::run(
        b.path(),
        &cb,
        with_hook(cmd.clone(), Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert_eq!(r, Outcome::Synced);
    let text = std::fs::read_to_string(&marker).unwrap();
    let mut lines = text.lines();
    let head = lines.next().unwrap();
    let canon = b.path().canonicalize().unwrap();
    assert_eq!(head, format!("3 0 {}", canon.display()), "{text}");
    let paths: Vec<&str> = lines.collect();
    assert_eq!(paths, ["dir", "dir/n.md", "top.md"]);
    // пустой цикл — хук не запускается
    std::fs::remove_file(&marker).unwrap();
    let r = runner::run(
        b.path(),
        &cb,
        with_hook(cmd.clone(), Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert_eq!(r, Outcome::Synced);
    assert!(!marker.exists());
    // своя отправка — тоже повод
    std::fs::write(b.path().join("from-b.md"), "z\n").unwrap();
    runner::run(b.path(), &cb, with_hook(cmd, Duration::from_secs(30)))
        .await
        .unwrap();
    let text = std::fs::read_to_string(&marker).unwrap();
    assert!(text.starts_with("0 1 "), "{text}");
    assert!(text.lines().any(|l| l == "from-b.md"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_failure_and_timeout_do_not_stop_sync() {
    let srv = server().await;
    let (a, ca) = folder(&srv, "hookfail", "a");
    let out = tempfile::tempdir().unwrap();
    let marker = out.path().join("late");
    std::fs::write(a.path().join("one.md"), "1\n").unwrap();
    let r = runner::run(
        a.path(),
        &ca,
        with_hook("exit 3".into(), Duration::from_secs(30)),
    )
    .await
    .unwrap();
    assert_eq!(r, Outcome::Synced, "код хука на исход не влияет");
    // таймаут: хук и его дочерние процессы убиты, синк завершён
    std::fs::write(a.path().join("two.md"), "2\n").unwrap();
    let cmd = format!("(sleep 3; touch '{}') & wait", marker.display());
    let t = std::time::Instant::now();
    let r = runner::run(a.path(), &ca, with_hook(cmd, Duration::from_millis(300)))
        .await
        .unwrap();
    assert_eq!(r, Outcome::Synced);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!marker.exists(), "дочерний процесс хука тоже остановлен");
}

#[tokio::test]
async fn exec_runs_coalesce_while_hook_is_busy() {
    use selfsync_cli::hook::{Batch, Hook, HookConfig};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".selfsync")).unwrap();
    let log = dir.path().join("runs.log");
    let cmd = format!(
        "sleep 0.5; echo \"$SELFSYNC_PULLED $(tr '\\n' ' ' < \"$SELFSYNC_CHANGES_FILE\")\" >> '{}'",
        log.display()
    );
    let hook = Hook::start(
        dir.path(),
        HookConfig {
            command: cmd,
            timeout: Duration::from_secs(10),
        },
    );
    let batch = |n: u64, p: &str| Batch {
        pulled: n,
        pushed: 0,
        paths: [p.to_owned()].into(),
    };
    hook.send(batch(1, "a.md"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    // два цикла, пока первый хук работает, — один повторный запуск
    hook.send(batch(2, "b.md"));
    hook.send(batch(3, "c.md"));
    hook.finish().await;
    let runs = std::fs::read_to_string(&log).unwrap();
    assert_eq!(runs, "1 a.md \n5 b.md c.md \n", "{runs}");
}

#[test]
fn git_excluded_by_default_for_new_configs() {
    let cfg = Config::default();
    let ex = selfsync_core::exclude::Excludes::new(&cfg.excludes);
    assert!(ex.is_excluded(".git/HEAD"));
    // старый конфиг с явным списком остаётся как был
    let old: Config = toml::from_str("excludes = [\".obsidian/workspace*.json\"]").unwrap();
    assert!(!selfsync_core::exclude::Excludes::new(&old.excludes).is_excluded(".git/HEAD"));
}
