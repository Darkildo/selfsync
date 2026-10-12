//! Служебные команды: import, gc, sweep, backup, token (через бинарь).

mod common;

use std::process::Command;
use std::time::{Duration, SystemTime};

use common::*;
use selfsync_core::blob;
use selfsync_core::hash::Hash;
use selfsync_proto::v1 as pb;
use selfsync_server::cmd;
use selfsync_server::db::vault::DAY_MS;
use selfsync_server::gc::{self, GcOptions};
use selfsync_server::state::open_vault;

fn age_file(path: &std::path::Path, days: u64) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(days * 86400))
        .unwrap();
}

#[tokio::test]
async fn import_existing_folder() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("dir/sub")).unwrap();
    std::fs::create_dir_all(src.path().join("empty")).unwrap();
    std::fs::write(src.path().join("root.md"), "# root").unwrap();
    std::fs::write(src.path().join("dir/sub/deep.md"), "deep").unwrap();
    // NFD-имя (как на macOS) нормализуется в NFC
    std::fs::write(src.path().join("Cafe\u{301}.md"), "café").unwrap();
    // недопустимое имя пропускается
    std::fs::write(src.path().join("trailing."), "x").unwrap();
    let r = cmd::import(s.config(), "notes", src.path(), 1 << 30).unwrap();
    assert_eq!(r.files, 3);
    assert_eq!(r.folders, 3, "dir, dir/sub, empty");
    assert_eq!(r.skipped.len(), 1);
    let ch = s.changes(&t, 0).await;
    let names: Vec<String> = ch
        .entries
        .iter()
        .map(|e| {
            e.path
                .as_ref()
                .unwrap()
                .segments
                .iter()
                .map(|s| String::from_utf8(s.clone()).unwrap())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    assert!(names.contains(&"Café.md".to_owned()));
    assert!(names.contains(&"empty".to_owned()));
    // содержимое — открытый блоб NSB
    let e = ch
        .entries
        .iter()
        .find(|e| e.path == Some(p("root.md")))
        .unwrap();
    let b = s
        .get(
            &format!("/v1/blobs/{}", Hash::from_slice(&e.hash).unwrap().to_hex()),
            &t,
        )
        .await;
    assert_eq!(blob::decode(&b.body, None).unwrap(), b"# root");
    // повторный импорт ничего не меняет
    let r2 = cmd::import(s.config(), "notes", src.path(), 1 << 30).unwrap();
    assert_eq!(r2.files, 0);
    assert_eq!(r2.unchanged, 3);
    // изменённый файл — новая ревизия
    std::fs::write(src.path().join("root.md"), "# root v2").unwrap();
    let r3 = cmd::import(s.config(), "notes", src.path(), 1 << 30).unwrap();
    assert_eq!(r3.files, 1);
}

#[tokio::test]
async fn gc_plan_is_dry_by_default_then_executes() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    for i in 0..25u64 {
        s.write_file(&t, "busy.md", i, format!("v{i}").as_bytes())
            .await;
    }
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let c = v.pool.get().unwrap();
    // вся история «старая»
    c.execute("UPDATE revisions SET created_at = 0", [])
        .unwrap();
    for (h, _, _) in v.blobs.list().unwrap() {
        age_file(&v.blobs.path_of(&h), 2);
    }
    // брошенная загрузка
    let st: pb::UploadState = s
        .post(
            "/v1/uploads",
            &t,
            &pb::UploadStart {
                hash: vec![5; 32],
                size: 10,
            },
        )
        .await
        .proto();
    c.execute("UPDATE uploads SET updated_at = 0", []).unwrap();
    // блоб без ссылок
    let (orphan, _) = s.put_blob(&t, b"orphan").await;
    age_file(&v.blobs.path_of(&orphan), 2);

    let now = selfsync_server::db::now_ms();
    let plan = gc::plan(&c, &v.blobs, GcOptions::default(), now).unwrap();
    assert_eq!(plan.revisions.len(), 5, "глубже 20 и старше 30 дней");
    assert_eq!(plan.blobs.len(), 6, "5 блобов вырезанных ревизий + сирота");
    assert_eq!(plan.uploads.len(), 1);
    // dry-run через команду: ничего не удалено
    drop(c);
    cmd::gc(s.config(), Some("notes"), GcOptions::default(), false).unwrap();
    assert!(v.blobs.exists(&orphan));
    assert!(v.blobs.upload_path(&st.upload_id).exists());
    // выполнение
    cmd::gc(s.config(), Some("notes"), GcOptions::default(), true).unwrap();
    assert!(!v.blobs.exists(&orphan));
    assert!(!v.blobs.upload_path(&st.upload_id).exists());
    let c = v.pool.get().unwrap();
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM revisions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 20);
    // текущая версия по-прежнему читается
    let ch = s.changes(&t, 0).await;
    let cur = Hash::from_slice(&ch.entries[0].hash).unwrap();
    assert_eq!(
        s.get(&format!("/v1/blobs/{}", cur.to_hex()), &t)
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn gc_keeps_fresh_unreferenced_blobs() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    // клиент загрузил блоб и ещё не отправил операцию
    let (fresh, _) = s.put_blob(&t, b"just uploaded").await;
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let c = v.pool.get().unwrap();
    let plan = gc::plan(
        &c,
        &v.blobs,
        GcOptions::default(),
        selfsync_server::db::now_ms(),
    )
    .unwrap();
    assert!(plan.blobs.is_empty());
    assert!(v.blobs.exists(&fresh));
}

#[tokio::test]
async fn sweep_erases_expired_deleted_files() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "old.md", 0, b"old").await;
    s.write_file(&t, "recent.md", 0, b"recent").await;
    s.ops(&t, vec![del_op("old.md", 1), del_op("recent.md", 1)])
        .await;
    let v = open_vault(s.config(), "notes", 1).unwrap();
    {
        let c = v.pool.get().unwrap();
        c.execute(
            "UPDATE files SET deleted_at = ?1 WHERE deleted = 1 AND seq = 3",
            [selfsync_server::db::now_ms() - 31 * DAY_MS],
        )
        .unwrap();
    }
    cmd::sweep(s.config()).unwrap();
    let d: pb::DeletedResponse = s.get("/v1/deleted", &t).await.proto();
    assert_eq!(d.items.len(), 1);
    assert_eq!(
        d.items[0].entry.as_ref().unwrap().path,
        Some(p("recent.md"))
    );
    assert_eq!(
        v.blobs.list().unwrap().len(),
        1,
        "блоб стёртого файла удалён"
    );
}

#[tokio::test]
async fn backup_is_consistent_copy() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"backed up").await;
    let dest = tempfile::tempdir().unwrap();
    let target = dest.path().join("bk");
    cmd::backup(s.config(), &target, Default::default()).unwrap();
    assert!(target.join("server.db").is_file());
    let copy = selfsync_server::Config::for_tests(target.clone());
    let v = open_vault(&copy, "notes", 1).unwrap();
    let c = v.pool.get().unwrap();
    let (entries, _) = selfsync_server::db::vault::changes(&c, 0, 10).unwrap();
    assert_eq!(entries.len(), 1);
    let h = Hash::from_slice(&entries[0].hash).unwrap();
    assert_eq!(
        blob::decode(&std::fs::read(v.blobs.path_of(&h)).unwrap(), None).unwrap(),
        b"backed up"
    );
    // токен работает и на копии
    let pool = selfsync_server::db::open(&target.join("server.db")).unwrap();
    assert!(
        selfsync_server::db::server::authenticate(&pool, &t)
            .unwrap()
            .is_some()
    );
    // повторно в тот же каталог — отказ
    assert!(cmd::backup(s.config(), &target, Default::default()).is_err());
}

#[test]
fn token_cli_roundtrip() {
    let bin = env!("CARGO_BIN_EXE_selfsync");
    let data = tempfile::tempdir().unwrap();
    let out = Command::new(bin)
        .args(["token", "add", "--data"])
        .arg(data.path())
        .args(["--vault", "notes", "--name", "laptop"])
        .env_remove("GATEWAY_INTERFACE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let token = text
        .split_whitespace()
        .find(|w| w.starts_with("ns_"))
        .unwrap();
    assert!(token.len() > 40);
    let list = Command::new(bin)
        .args(["token", "list", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    let l = String::from_utf8(list.stdout).unwrap();
    assert!(l.contains("laptop"));
    assert!(l.contains("клиент неизвестен"), "{l}");
    assert!(!l.contains(token), "токен не показывается повторно");
    let rv = Command::new(bin)
        .args(["token", "revoke", "--name", "laptop", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(rv.status.success());
    let list = Command::new(bin)
        .args(["token", "list", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(String::from_utf8(list.stdout).unwrap().contains("ОТОЗВАН"));
    let vl = Command::new(bin)
        .args(["vault", "list", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(String::from_utf8(vl.stdout).unwrap().contains("notes"));
    let link = Command::new(bin)
        .args([
            "link",
            "--vault",
            "notes",
            "--name",
            "phone",
            "--url",
            "https://n.example",
            "--data",
        ])
        .arg(data.path())
        .output()
        .unwrap();
    let lt = String::from_utf8(link.stdout).unwrap();
    assert!(lt.contains("https://n.example/join/"));
    assert!(lt.contains('█'), "QR в терминале");
}

#[test]
fn sweep_cleans_old_throttle_rows() {
    let s = TestServer::new();
    let now = selfsync_server::db::now_ms();
    {
        let pool = s.state.server_pool().unwrap();
        let c = pool.get().unwrap();
        c.execute(
            "INSERT INTO throttle (key, window_start, failures, blocked_until) VALUES
               ('redeem:old', ?1, 3, NULL),
               ('redeem:blocked', ?1, 10, ?2),
               ('redeem:fresh', ?3, 1, NULL)",
            rusqlite::params![now - 2 * DAY_MS, now + 3_600_000, now],
        )
        .unwrap();
    }
    cmd::sweep(s.config()).unwrap();
    let pool = s.state.server_pool().unwrap();
    let c = pool.get().unwrap();
    let mut st = c.prepare("SELECT key FROM throttle ORDER BY key").unwrap();
    let keys: Vec<String> = st
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(keys, ["redeem:blocked", "redeem:fresh"]);
}

/// Добавка лимитера на горячем пути авторизации — один `SELECT` по ключу. Замер:
/// `cargo test --release -p selfsync-server --test commands throttle_check_cost -- --ignored --nocapture`.
#[test]
#[ignore = "замер, не проверка"]
fn throttle_check_cost() {
    use selfsync_server::throttle;
    let s = TestServer::new();
    let pool = s.state.server_pool().unwrap();
    let c = pool.get().unwrap();
    let rate = s.config().rate;
    // таблица с тысячей чужих адресов, наш — без записи и с записью
    for i in 0..1000 {
        let ip = format!("10.0.{}.{}", i / 256, i % 256).parse().ok();
        throttle::fail_now(&c, &throttle::auth_keys(&rate, ip)).unwrap();
    }
    let n = 20_000;
    for ip in ["192.0.2.1", "10.0.0.1"] {
        let keys = throttle::auth_keys(&rate, ip.parse().ok());
        let t = std::time::Instant::now();
        for _ in 0..n {
            throttle::check_now(&c, &keys).unwrap();
        }
        let per = t.elapsed() / n;
        println!("check {ip}: {per:?} на запрос");
        assert!(per < Duration::from_micros(100), "{per:?}");
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

use selfsync_server::verify::{self, VerifyOptions};

/// Хэш блоба, который `write_file` кладёт для этого содержимого.
fn blob_hash(content: &[u8]) -> Hash {
    Hash::of(&blob::encode(content, None))
}

/// Испортить байт на месте (тот же inode — как «тихая» порча диска).
fn flip_byte(path: &std::path::Path) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(3)).unwrap();
    f.write_all(b"\xff").unwrap();
}

/// Заменить файл другим содержимым (новый inode: жёсткие ссылки бэкапа не задеты).
fn replace_file(path: &std::path::Path, data: &[u8]) {
    std::fs::remove_file(path).unwrap();
    std::fs::write(path, data).unwrap();
}

fn run_verify(data: &std::path::Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_selfsync"))
        .arg("verify")
        .args(args)
        .arg("--data")
        .arg(data)
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap(), text)
}

async fn two_files(s: &TestServer) {
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"first file").await;
    s.write_file(&t, "dir/b.md", 0, b"second file").await;
}

#[tokio::test]
async fn verify_clean_store_is_ok() {
    let s = TestServer::new();
    two_files(&s).await;
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert!(r.clean(), "{r:?}");
    assert_eq!(r.blobs, 2);
    assert!(r.bytes > 0);
    assert_eq!((r.orphans, r.uploads), (0, 0));
    let (code, out) = run_verify(&s.config().data_dir, &[]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("server.db: в порядке"), "{out}");
    assert!(out.contains("vault notes: блобов 2"), "{out}");
    // --quiet на чистом хранилище молчит
    let (code, out) = run_verify(&s.config().data_dir, &["--quiet"]);
    assert_eq!((code, out.as_str()), (0, ""));
}

#[tokio::test]
async fn verify_detects_corrupt_blob() {
    let s = TestServer::new();
    two_files(&s).await;
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let h = blob_hash(b"first file");
    flip_byte(&v.blobs.path_of(&h));
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert_eq!(r.corrupt, vec![h]);
    assert!(!r.clean());
    // битый — в карантине и запрошен у устройств
    assert!(!v.blobs.exists(&h));
    assert!(v.dir.join("quarantine").join(h.to_hex()).is_file());
    assert_eq!(r.wanted.iter().map(|w| w.0).collect::<Vec<_>>(), vec![h]);
    // повторная проверка видит его отсутствующим, запрос остаётся
    let (code, out) = run_verify(&s.config().data_dir, &["--quiet"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains(&format!("нет блоба {}", h.to_hex())), "{out}");
    assert!(out.contains("запрошено у устройств 1"), "{out}");
    assert!(
        !out.contains("server.db"),
        "--quiet печатает только проблемы: {out}"
    );
}

#[tokio::test]
async fn wanted_blob_is_settled_by_upload() {
    let s = TestServer::new();
    two_files(&s).await;
    let t = s.token("notes", "b");
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let h = blob_hash(b"first file");
    let before = s.changes(&t, 0).await.vault.unwrap();
    assert_eq!((before.verified_at, before.storage_problem), (0, false));
    flip_byte(&v.blobs.path_of(&h));
    verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    let vs = s.changes(&t, 0).await.vault.unwrap();
    assert_eq!(vs.blobs_wanted, 1);
    assert!(vs.verified_at > 0, "время проверки доходит до устройств");
    assert!(!vs.storage_problem, "битый блоб чинят устройства, не админ");
    let wanted: pb::HashList = s.get("/v1/blobs/wanted", &t).await.proto();
    assert_eq!(wanted.hashes, vec![h.to_vec()]);
    // чужие байты под этим именем не принимаются, запрос остаётся
    let r = s
        .call(
            http::Method::PUT,
            &format!("/v1/blobs/{}", h.to_hex()),
            Some(&t),
            b"not it".to_vec(),
            &[],
        )
        .await;
    assert_eq!(r.status, http::StatusCode::BAD_REQUEST);
    assert_eq!(s.changes(&t, 0).await.vault.unwrap().blobs_wanted, 1);
    // целый блоб снимает запрос
    s.put_blob(&t, &blob::encode(b"first file", None)).await;
    assert!(v.blobs.exists(&h));
    assert_eq!(s.changes(&t, 0).await.vault.unwrap().blobs_wanted, 0);
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert!(r.clean() && r.wanted.is_empty(), "{r:?}");
    // запись ключа не читается — это чинит только админ
    {
        let c = v.pool.get().unwrap();
        selfsync_server::db::vault::set_meta_int(&c, "verify_key_problem", 1).unwrap();
    }
    assert!(s.changes(&t, 0).await.vault.unwrap().storage_problem);
}

#[tokio::test]
async fn verify_detects_missing_blob() {
    let s = TestServer::new();
    two_files(&s).await;
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let h = blob_hash(b"second file");
    std::fs::remove_file(v.blobs.path_of(&h)).unwrap();
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert_eq!(r.missing.len(), 1);
    assert_eq!(r.missing[0].hash, h);
    assert_eq!(r.missing[0].paths, ["dir/b.md"]);
    let (code, out) = run_verify(&s.config().data_dir, &[]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains(&format!("нет блоба {} (dir/b.md)", h.to_hex())),
        "{out}"
    );
    assert!(out.contains("--repair-from"), "подсказка про бэкап: {out}");
}

#[tokio::test]
async fn verify_repairs_from_backup() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    two_files(&s).await;
    let bk = tempfile::tempdir().unwrap();
    let bk = bk.path().join("bk");
    cmd::backup(s.config(), &bk, Default::default()).unwrap();
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let a = blob_hash(b"first file");
    let b = blob_hash(b"second file");
    replace_file(&v.blobs.path_of(&a), b"garbage");
    std::fs::remove_file(v.blobs.path_of(&b)).unwrap();
    let (code, out) = run_verify(
        &s.config().data_dir,
        &["--repair-from", bk.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(&format!("восстановлен из бэкапа {}", a.to_hex())),
        "{out}"
    );
    // битый оригинал — в карантине, на месте — проверенная копия
    let q = v.dir.join("quarantine").join(a.to_hex());
    assert_eq!(std::fs::read(&q).unwrap(), b"garbage");
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert!(r.clean(), "{r:?}");
    // клиент снова получает файл
    let got = s
        .call(
            http::Method::GET,
            &format!("/v1/blobs/{}", b.to_hex()),
            Some(&t),
            vec![],
            &[],
        )
        .await;
    assert_eq!(
        got.body.as_ref(),
        blob::encode(b"second file", None).as_slice()
    );
    // gc карантин не трогает, backup его копирует
    cmd::gc(s.config(), None, GcOptions::default(), true).unwrap();
    assert!(q.is_file());
    let bk2 = tempfile::tempdir().unwrap();
    cmd::backup(s.config(), &bk2.path().join("bk"), Default::default()).unwrap();
    assert!(
        bk2.path()
            .join("bk/vaults/notes/quarantine")
            .join(a.to_hex())
            .is_file()
    );
}

#[tokio::test]
async fn verify_rejects_bad_backup_blob() {
    let s = TestServer::new();
    two_files(&s).await;
    let bk = tempfile::tempdir().unwrap();
    let bk = bk.path().join("bk");
    cmd::backup(s.config(), &bk, Default::default()).unwrap();
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let a = blob_hash(b"first file");
    let hex = a.to_hex();
    replace_file(&v.blobs.path_of(&a), b"garbage");
    replace_file(
        &bk.join("vaults/notes/blobs").join(&hex[..2]).join(&hex),
        b"also garbage",
    );
    let opts = VerifyOptions {
        repair_from: Some(&bk),
        progress: false,
    };
    let r = verify::verify_vault(s.config(), "notes", opts).unwrap();
    assert_eq!(r.corrupt, vec![a]);
    assert_eq!(r.unrepaired, vec![(a, "в бэкапе тоже повреждён")]);
    assert!(r.repaired.is_empty());
    // оригинал — в карантине, блоб запрошен у устройств
    assert!(!v.blobs.exists(&a));
    assert_eq!(
        std::fs::read(v.dir.join("quarantine").join(&hex)).unwrap(),
        b"garbage"
    );
    assert_eq!(r.wanted.len(), 1);
    let (code, out) = run_verify(
        &s.config().data_dir,
        &["--quiet", "--repair-from", bk.to_str().unwrap()],
    );
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("не восстановлен"), "{out}");
}

#[tokio::test]
async fn verify_checks_vault_key_record() {
    let s = TestServer::new();
    two_files(&s).await;
    let v = open_vault(s.config(), "notes", 1).unwrap();
    {
        let c = v.pool.get().unwrap();
        selfsync_server::db::vault::set_meta_blob(&c, "vault_key", b"not a record").unwrap();
    }
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert!(r.key_problem.as_deref().unwrap().contains("не разбирается"));
    {
        let c = v.pool.get().unwrap();
        c.execute("DELETE FROM meta WHERE key = 'vault_key'", [])
            .unwrap();
        // зашифрованный путь без записи ключа
        c.execute(
            "UPDATE files SET path = CAST(x'01' || substr(path, 2) AS BLOB) WHERE path = (SELECT MIN(path) FROM files)",
            [],
        )
        .unwrap();
    }
    let r = verify::verify_vault(s.config(), "notes", VerifyOptions::default()).unwrap();
    assert!(
        r.key_problem
            .as_deref()
            .unwrap()
            .contains("нет записи ключа")
    );
}

#[tokio::test]
async fn verify_runs_alongside_uploads() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    for i in 0..50 {
        s.write_file(&t, &format!("n{i}.md"), 0, format!("note {i}").as_bytes())
            .await;
    }
    // незавершённая загрузка: часть уже в uploads/
    let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let h = Hash::of(&data);
    let st: pb::UploadState = s
        .post(
            "/v1/uploads",
            &t,
            &pb::UploadStart {
                hash: h.to_vec(),
                size: data.len() as u64,
            },
        )
        .await
        .proto();
    let id = st.upload_id;
    let r = s
        .call(
            http::Method::PUT,
            &format!("/v1/uploads/{id}"),
            Some(&t),
            data[..10_000].to_vec(),
            &[("content-range", "bytes 0-9999/20000")],
        )
        .await;
    assert!(r.status.is_success());
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let part = std::fs::read(v.blobs.upload_path(&id)).unwrap();
    // обход в другом потоке, сервер тем временем пишет
    let config = s.config().clone();
    let walker = std::thread::spawn(move || {
        verify::verify_vault(&config, "notes", VerifyOptions::default()).unwrap()
    });
    for i in 0..20 {
        let r = s
            .write_file(
                &t,
                &format!("live{i}.md"),
                0,
                format!("live {i}").as_bytes(),
            )
            .await;
        assert!(matches!(r.result, Some(pb::op_result::Result::Applied(_))));
    }
    let r = walker.join().unwrap();
    assert!(r.clean(), "{r:?}");
    assert_eq!(std::fs::read(v.blobs.upload_path(&id)).unwrap(), part);
    // загрузка продолжается как ни в чём не бывало
    let r = s
        .call(
            http::Method::PUT,
            &format!("/v1/uploads/{id}"),
            Some(&t),
            data[10_000..].to_vec(),
            &[("content-range", "bytes 10000-19999/20000")],
        )
        .await;
    assert!(r.status.is_success());
    let r = s
        .call(
            http::Method::POST,
            &format!("/v1/uploads/{id}/commit"),
            Some(&t),
            vec![],
            &[],
        )
        .await;
    assert!(r.status.is_success(), "{}", r.status);
    assert!(v.blobs.exists(&h));
}

#[test]
fn verify_start_errors_exit_2() {
    let data = tempfile::tempdir().unwrap();
    let (code, out) = run_verify(data.path(), &["--vault", "nope"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("не найден"), "{out}");
    let (code, _) = run_verify(data.path(), &["--repair-from", "/nonexistent/backup"]);
    assert_eq!(code, 2);
}

#[cfg(unix)]
fn inode(p: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().ino()
}

/// Без `--copy` порча байта в хранилище портит и копию (общий inode); с `--copy` —
/// нет, и `verify --repair-from` лечит хранилище из неё.
#[cfg(unix)]
#[tokio::test]
async fn backup_copy_survives_bit_rot() {
    let s = TestServer::new();
    two_files(&s).await;
    let v = open_vault(s.config(), "notes", 1).unwrap();
    let a = blob_hash(b"first file");
    let hex = a.to_hex();
    let rel = format!("vaults/notes/blobs/{}/{hex}", &hex[..2]);
    let dir = tempfile::tempdir().unwrap();
    let linked = dir.path().join("linked");
    let copied = dir.path().join("copied");
    cmd::backup(s.config(), &linked, Default::default()).unwrap();
    cmd::backup(
        s.config(),
        &copied,
        cmd::BlobSource {
            copy: true,
            previous: None,
        },
    )
    .unwrap();
    let store = v.blobs.path_of(&a);
    assert_eq!(inode(&store), inode(&linked.join(&rel)));
    assert_ne!(inode(&store), inode(&copied.join(&rel)));
    flip_byte(&store);
    assert_ne!(Hash::of(&std::fs::read(linked.join(&rel)).unwrap()), a);
    assert_eq!(Hash::of(&std::fs::read(copied.join(&rel)).unwrap()), a);
    // копию можно проверить как хранилище
    let (code, out) = run_verify(&copied, &[]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_verify(
        &s.config().data_dir,
        &["--repair-from", copied.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(Hash::of(&std::fs::read(&store).unwrap()), a);
}

/// Ротация: остаётся `keep` последних, недописанная копия не считается и
/// убирается, блобы между копиями — общие, с хранилищем — нет.
#[cfg(unix)]
#[tokio::test]
async fn backup_rotation_keeps_last_n() {
    let s = TestServer::new();
    two_files(&s).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    // остатки прерванного запуска и чужой каталог
    std::fs::create_dir_all(root.join(".2020-01-01T00-00-00Z.partial/vaults")).unwrap();
    std::fs::create_dir_all(root.join("keep-me")).unwrap();
    for _ in 0..3 {
        cmd::backup_rotate(s.config(), root, 2, true).unwrap();
    }
    let list = cmd::rotation_list(root).unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    assert!(!root.join(".2020-01-01T00-00-00Z.partial").exists());
    assert!(root.join("keep-me").is_dir());
    let a = blob_hash(b"first file");
    let hex = a.to_hex();
    let rel = format!("vaults/notes/blobs/{}/{hex}", &hex[..2]);
    let v = open_vault(s.config(), "notes", 1).unwrap();
    assert_eq!(inode(&list[0].join(&rel)), inode(&list[1].join(&rel)));
    assert_ne!(inode(&v.blobs.path_of(&a)), inode(&list[1].join(&rel)));
    for b in &list {
        let (code, out) = run_verify(b, &[]);
        assert_eq!(code, 0, "{}: {out}", b.display());
    }
    assert!(cmd::backup_rotate(s.config(), root, 0, true).is_err());
}
