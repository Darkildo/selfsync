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
    cmd::backup(s.config(), &target).unwrap();
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
    assert!(cmd::backup(s.config(), &target).is_err());
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
