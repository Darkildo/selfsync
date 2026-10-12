//! Квоты (F16): байты хранилища, устройства, размер файла, потолок сервера и стирание
//! старых версий.

mod common;

use common::*;
use http::{Method, StatusCode};
use prost::Message;
use selfsync_core::blob;
use selfsync_core::hash::Hash;
use selfsync_proto::v1 as pb;
use selfsync_server::quota::{self, Own};
use selfsync_server::state::open_vault;
use selfsync_server::{Config, Mode};

fn set_quota(s: &TestServer, vault: &str, own: Own) {
    let v = open_vault(s.config(), vault, 1).unwrap();
    quota::set_own(&v.pool.get().unwrap(), &own).unwrap();
}

async fn state(s: &TestServer, t: &str) -> pb::VaultState {
    s.changes(t, 0).await.vault.unwrap()
}

/// Блоб без `Content-Length`: размер сервер узнаёт по ходу записи.
async fn put_raw(s: &TestServer, t: &str, data: &[u8]) -> Resp {
    s.call(
        Method::PUT,
        &format!("/v1/blobs/{}", Hash::of(data).to_hex()),
        Some(t),
        data.to_vec(),
        &[],
    )
    .await
}

async fn put_sized(s: &TestServer, t: &str, data: &[u8]) -> Resp {
    let len = data.len().to_string();
    s.call(
        Method::PUT,
        &format!("/v1/blobs/{}", Hash::of(data).to_hex()),
        Some(t),
        data.to_vec(),
        &[("content-length", len.as_str())],
    )
    .await
}

async fn upload_start(s: &TestServer, t: &str, data: &[u8]) -> Resp {
    s.post(
        "/v1/uploads",
        t,
        &pb::UploadStart {
            hash: Hash::of(data).to_vec(),
            size: data.len() as u64,
        },
    )
    .await
}

#[tokio::test]
async fn hard_limit_refuses_new_blobs_only() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"first").await;
    let used = blob::encode(b"first", None).len() as u64;
    let vs = state(&s, &t).await;
    assert_eq!(vs.used_bytes, used);
    assert_eq!((vs.quota_soft, vs.quota_hard), (0, 0));
    assert_eq!(vs.max_file_size, s.config().max_blob_size);

    set_quota(
        &s,
        "notes",
        Own {
            soft: Some(used),
            hard: Some(used + 30),
            ..Own::default()
        },
    );
    let vs = state(&s, &t).await;
    assert_eq!((vs.quota_soft, vs.quota_hard), (used, used + 30));

    // Больше порога — отказ, и с Content-Length, и без него.
    let big = vec![7u8; 40];
    let r = put_sized(&s, &t, &big).await;
    assert_eq!(r.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(r.error().code, "quota_exceeded");
    let r = put_raw(&s, &t, &big).await;
    assert_eq!(
        r.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "без длины — по ходу записи"
    );
    assert!(
        !s.state
            .vault("notes")
            .unwrap()
            .blobs
            .exists(&Hash::of(&big))
    );
    let r = upload_start(&s, &t, &big).await;
    assert_eq!(r.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(state(&s, &t).await.used_bytes, used, "отказ места не занял");

    // Умещается — принимается и учитывается.
    let small = vec![1u8; 20];
    assert_eq!(put_raw(&s, &t, &small).await.status, StatusCode::CREATED);
    assert_eq!(state(&s, &t).await.used_bytes, used + 20);
    // Уже лежащий блоб и операции без новых блобов работают и на полном vault'е.
    assert_eq!(put_raw(&s, &t, &small).await.status, StatusCode::OK);
    let r = s
        .ops(&t, vec![put_op("b.md", 0, Hash::of(&small), 20)])
        .await;
    applied(&r.results[0]);
    let r = s.ops(&t, vec![ren_op("a.md", "c.md", 1)]).await;
    applied(&r.results[0]);
    let r = s.ops(&t, vec![del_op("b.md", 1)]).await;
    applied(&r.results[0]);
    let r = s
        .get(&format!("/v1/blobs/{}", Hash::of(&small).to_hex()), &t)
        .await;
    assert_eq!(r.status, StatusCode::OK, "скачивание работает");
}

#[tokio::test]
async fn wanted_blob_is_accepted_over_quota() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"repair me").await;
    let b = blob::encode(b"repair me", None);
    let h = Hash::of(&b);
    let v = s.state.vault("notes").unwrap();
    std::fs::remove_file(v.blobs.path_of(&h)).unwrap();
    selfsync_server::verify::verify_vault(s.config(), "notes", Default::default()).unwrap();
    assert_eq!(
        state(&s, &t).await.used_bytes,
        0,
        "verify пересчитал по диску"
    );
    set_quota(
        &s,
        "notes",
        Own {
            hard: Some(1),
            ..Own::default()
        },
    );
    assert_eq!(
        put_raw(&s, &t, b"other").await.status,
        StatusCode::INSUFFICIENT_STORAGE
    );
    assert_eq!(
        put_raw(&s, &t, &b).await.status,
        StatusCode::CREATED,
        "починка — не новые данные"
    );
    assert!(v.blobs.exists(&h));
}

#[tokio::test]
async fn max_file_per_vault() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    set_quota(
        &s,
        "notes",
        Own {
            max_file: Some(100),
            ..Own::default()
        },
    );
    assert_eq!(state(&s, &t).await.max_file_size, 100);
    let r = put_raw(&s, &t, &[0u8; 101]).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    let r = upload_start(&s, &t, &[0u8; 101]).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    let ok = [0u8; 60];
    assert_eq!(put_raw(&s, &t, &ok).await.status, StatusCode::CREATED);
    let r = s
        .ops(&t, vec![put_op("x.bin", 0, Hash::of(&ok), 101)])
        .await;
    assert!(
        matches!(&r.results[0].result, Some(pb::op_result::Result::Rejected(x)) if x.code == "too_large"),
        "{r:?}"
    );
    // Лимит vault'а не поднимает лимит сервера.
    set_quota(
        &s,
        "notes",
        Own {
            max_file: Some(u64::MAX / 2),
            ..Own::default()
        },
    );
    assert_eq!(state(&s, &t).await.max_file_size, s.config().max_blob_size);
}

#[tokio::test]
async fn defaults_from_config_and_server_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::for_tests(dir.path().to_path_buf());
    config.quota.defaults.soft = 1000;
    config.quota.server_hard = 60;
    let s = TestServer::with_config(dir, config, Mode::Serve);
    let a = s.token("one", "a");
    let b = s.token("two", "b");
    assert_eq!(
        state(&s, &a).await.quota_soft,
        1000,
        "умолчание конфигурации"
    );
    // Свой лимит vault'а сильнее умолчания, `0` — без порога.
    set_quota(
        &s,
        "two",
        Own {
            soft: Some(0),
            ..Own::default()
        },
    );
    assert_eq!(state(&s, &b).await.quota_soft, 0);
    assert_eq!(
        put_raw(&s, &a, &[1u8; 40]).await.status,
        StatusCode::CREATED
    );
    // Потолок сервера — на все vault'ы вместе.
    let r = put_raw(&s, &b, &[2u8; 30]).await;
    assert_eq!(r.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(
        put_raw(&s, &b, &[2u8; 20]).await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn device_limit() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    set_quota(
        &s,
        "notes",
        Own {
            devices: Some(2),
            ..Own::default()
        },
    );
    // Код, выписанный при свободном месте, не подключит сверх лимита.
    let code: pb::JoinCode = s
        .post("/v1/join", &t, &pb::JoinCreate::default())
        .await
        .proto();
    let r = s
        .post("/v1/join", &t, &pb::JoinCreate::default())
        .await
        .proto::<pb::JoinCode>();
    let _second = s.token("notes", "b");
    let redeem = |code: String| {
        let body = pb::JoinRedeem {
            code,
            name: "c".into(),
        }
        .encode_to_vec();
        s.call(Method::POST, "/v1/join/redeem", None, body, &[])
    };
    let res = redeem(code.code).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(res.error().code, "device_limit");
    let res = s.post("/v1/join", &t, &pb::JoinCreate::default()).await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "ссылку при полном vault'е не выписать"
    );
    // Отозванное место освобождает.
    let c = s.state.server_pool().unwrap().get().unwrap();
    selfsync_server::db::server::revoke_by_name(&c, Some("notes"), "b").unwrap();
    assert_eq!(redeem(r.code).await.status, StatusCode::OK);
    // Админ упирается в тот же лимит.
    let config = s.config().clone();
    let e = selfsync_server::cmd::token_add(&config, "notes", "d", false).unwrap_err();
    assert!(e.to_string().contains("лимит"), "{e}");
}

async fn purge_history(s: &TestServer, t: &str, path: &str, before_rev: u64) -> Resp {
    let req = pb::HistoryPurge {
        path: Some(p(path)),
        before_rev,
    };
    s.post("/v1/history/purge", t, &req).await
}

#[tokio::test]
async fn history_purge_frees_space() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    for (rev, text) in [b"one".as_slice(), b"two", b"three"].iter().enumerate() {
        s.write_file(&t, "h.md", rev as u64, text).await;
    }
    let used = state(&s, &t).await.used_bytes;
    let r: pb::PurgeResult = purge_history(&s, &t, "h.md", 3).await.proto();
    assert_eq!(r.purged, 2);
    let q = {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(p("h.md").encode_to_vec())
    };
    let h: pb::HistoryResponse = s.get(&format!("/v1/history?path={q}"), &t).await.proto();
    assert_eq!(h.revisions.iter().map(|r| r.rev).collect::<Vec<_>>(), [3]);
    let old = Hash::of(&blob::encode(b"one", None));
    assert!(!s.state.vault("notes").unwrap().blobs.exists(&old));
    let freed = 2 * blob::encode(b"one", None).len() as u64;
    assert_eq!(state(&s, &t).await.used_bytes, used - freed);

    // Удалённый файл: последняя живая версия остаётся — её восстанавливают из корзины.
    s.ops(&t, vec![del_op("h.md", 3)]).await;
    let r: pb::PurgeResult = purge_history(&s, &t, "h.md", u64::MAX).await.proto();
    assert_eq!(r.purged, 0);
    let d: pb::DeletedResponse = s.get("/v1/deleted", &t).await.proto();
    assert_eq!(d.items.len(), 1);

    // Блоб, на который ссылается другой файл, не удаляется.
    s.write_file(&t, "x.md", 0, b"shared").await;
    s.write_file(&t, "x.md", 1, b"next").await;
    s.write_file(&t, "y.md", 0, b"shared").await;
    let r: pb::PurgeResult = purge_history(&s, &t, "x.md", 2).await.proto();
    assert_eq!(r.purged, 1);
    let shared = Hash::of(&blob::encode(b"shared", None));
    assert!(s.state.vault("notes").unwrap().blobs.exists(&shared));

    // Устройство только для чтения стирать не может.
    let ro = s.token_read_only("notes", "ro");
    let r = purge_history(&s, &ro, "x.md", 2).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn usage_follows_trash_purge_and_verify() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"bye").await;
    let used = state(&s, &t).await.used_bytes;
    assert!(used > 0);
    s.ops(&t, vec![del_op("a.md", 1)]).await;
    let r: pb::PurgeResult = s
        .call(
            Method::DELETE,
            "/v1/deleted",
            Some(&t),
            pb::PathList {
                paths: vec![p("a.md")],
            }
            .encode_to_vec(),
            &[],
        )
        .await
        .proto();
    assert_eq!(r.purged, 1);
    assert_eq!(state(&s, &t).await.used_bytes, 0);
    // Расхождение счётчика с диском исправляет verify.
    s.write_file(&t, "b.md", 0, b"again").await;
    let v = s.state.vault("notes").unwrap();
    quota::set_usage(&v.pool.get().unwrap(), 12345).unwrap();
    selfsync_server::verify::verify_vault(s.config(), "notes", Default::default()).unwrap();
    assert_eq!(
        state(&s, &t).await.used_bytes,
        blob::encode(b"again", None).len() as u64
    );
}

#[tokio::test]
async fn vault_opened_first_time_counts_existing_blobs() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"present").await;
    let v = s.state.vault("notes").unwrap();
    v.pool
        .get()
        .unwrap()
        .execute("DELETE FROM meta WHERE key = 'usage_bytes'", [])
        .unwrap();
    // Новый процесс (CGI, перезапуск) заводит счётчик по диску.
    let fresh = open_vault(s.config(), "notes", 1).unwrap();
    assert_eq!(
        quota::usage(&fresh.pool.get().unwrap()).unwrap(),
        blob::encode(b"present", None).len() as u64
    );
}

#[tokio::test]
async fn admin_sets_and_resets_limits() {
    use selfsync_server::cmd::{QuotaPatch, vault_quota};
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::for_tests(dir.path().to_path_buf());
    config.quota.defaults.devices = 5;
    let set = |soft: Option<&str>, hard: Option<&str>, devices: Option<&str>| QuotaPatch {
        soft: soft.map(str::to_owned),
        hard: hard.map(str::to_owned),
        devices: devices.map(str::to_owned),
        max_file: None,
    };
    vault_quota(
        &config,
        "notes",
        &set(Some("1GiB"), Some("2GiB"), Some("0")),
    )
    .unwrap();
    let v = open_vault(&config, "notes", 1).unwrap();
    let c = v.pool.get().unwrap();
    let l = quota::limits(&c, &config.quota.defaults).unwrap();
    assert_eq!((l.soft, l.hard, l.devices), (1 << 30, 2 << 30, 0));
    // Без флага — не меняется; `default` — снова как в конфигурации.
    vault_quota(&config, "notes", &set(None, None, Some("default"))).unwrap();
    let l = quota::limits(&c, &config.quota.defaults).unwrap();
    assert_eq!((l.soft, l.devices), (1 << 30, 5));
    assert!(vault_quota(&config, "notes", &set(Some("3GiB"), None, None)).is_err());
    assert!(vault_quota(&config, "notes", &set(None, None, Some("many"))).is_err());
    assert!(vault_quota(&config, "Bad", &set(None, None, None)).is_err());
}
