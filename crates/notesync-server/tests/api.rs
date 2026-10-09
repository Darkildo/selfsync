//! Интеграционные тесты HTTP API (раздел 13.3): общий Router через `oneshot`.

mod common;

use std::time::Duration;

use base64::Engine as _;
use common::*;
use http::{Method, StatusCode};
use notesync_core::hash::Hash;
use notesync_proto::v1 as pb;
use notesync_server::Mode;
use prost::Message;

#[tokio::test]
async fn health_is_json_without_auth() {
    let s = TestServer::new();
    let r = s
        .send(
            http::Request::get("/v1/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["proto"], 1);
}

#[tokio::test]
async fn auth_and_proto_header() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    // без токена
    let r = s.call(Method::GET, "/v1/changes", None, vec![], &[]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // неизвестный токен
    let r = s.get("/v1/changes", "ns_nope").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // нет заголовка версии
    let r = s
        .send(
            http::Request::get("/v1/changes")
                .header("authorization", format!("Bearer {t}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error().code, "proto_header_missing");
    // несовместимая версия
    let r = s
        .send(
            http::Request::get("/v1/changes")
                .header("authorization", format!("Bearer {t}"))
                .header("x-notesync-proto", "2")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(r.error().supported_proto, 1);
    // всё в порядке
    assert_eq!(s.get("/v1/changes", &t).await.status, StatusCode::OK);
}

#[tokio::test]
async fn revoked_token_gets_401() {
    let s = TestServer::new();
    let t = s.token("notes", "phone");
    let other = s.token("notes", "laptop");
    let devs: pb::DevicesResponse = s.get("/v1/devices", &other).await.proto();
    let phone = devs.devices.iter().find(|d| d.name == "phone").unwrap().id;
    let r = s
        .call(
            Method::DELETE,
            &format!("/v1/devices/{phone}"),
            Some(&other),
            vec![],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.get("/v1/changes", &t).await.status,
        StatusCode::UNAUTHORIZED
    );
    let devs: pb::DevicesResponse = s.get("/v1/devices", &other).await.proto();
    assert!(devs.devices.iter().find(|d| d.id == phone).unwrap().revoked);
}

#[tokio::test]
async fn ops_check_order_and_noop_seq() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let r = s.write_file(&t, "a.md", 0, b"one").await;
    assert_eq!(
        applied(&r),
        pb::Applied {
            rev: 1,
            seq: 1,
            noop: false
        }
    );
    // повтор после «обрыва»: старый base_rev, тот же хэш — no-op, seq не двигается
    let r = s.write_file(&t, "a.md", 0, b"one").await;
    assert_eq!(
        applied(&r),
        pb::Applied {
            rev: 1,
            seq: 1,
            noop: true
        }
    );
    assert_eq!(s.changes(&t, 0).await.vault.unwrap().seq, 1);
    // другой хэш со старым base_rev — конфликт с текущей записью
    let r = s.write_file(&t, "a.md", 0, b"two").await;
    match r.result {
        Some(pb::op_result::Result::Conflict(c)) => assert_eq!(c.server.unwrap().rev, 1),
        o => panic!("{o:?}"),
    }
    // блоба нет — MissingBlob раньше проверки base_rev
    let resp = s
        .ops(&t, vec![put_op("a.md", 99, Hash::of(b"never uploaded"), 5)])
        .await;
    assert!(matches!(
        resp.results[0].result,
        Some(pb::op_result::Result::MissingBlob(_))
    ));
}

#[tokio::test]
async fn blob_upload_missing_and_hash_mismatch() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let (h, r) = s.put_blob(&t, b"content").await;
    assert_eq!(r.status, StatusCode::CREATED);
    let (_, r) = s.put_blob(&t, b"content").await;
    assert_eq!(r.status, StatusCode::OK);
    let missing: pb::HashList = s
        .post(
            "/v1/blobs/missing",
            &t,
            &pb::HashList {
                hashes: vec![h.to_vec(), vec![7; 32]],
            },
        )
        .await
        .proto();
    assert_eq!(missing.hashes, vec![vec![7u8; 32]]);
    let wrong = Hash::of(b"other");
    let r = s
        .call(
            Method::PUT,
            &format!("/v1/blobs/{}", wrong.to_hex()),
            Some(&t),
            b"content".to_vec(),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error().code, "hash_mismatch");
}

#[tokio::test]
async fn blob_range_etag_head() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let (h, _) = s.put_blob(&t, &data).await;
    let uri = format!("/v1/blobs/{}", h.to_hex());
    let r = s.get(&uri, &t).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], &data[..]);
    let etag = r.headers["etag"].to_str().unwrap().to_owned();
    let r = s
        .call(
            Method::GET,
            &uri,
            Some(&t),
            vec![],
            &[("range", "bytes=100-199")],
        )
        .await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&r.body[..], &data[100..200]);
    assert_eq!(r.headers["content-range"], "bytes 100-199/1000");
    let r = s
        .call(
            Method::GET,
            &uri,
            Some(&t),
            vec![],
            &[("range", "bytes=-10")],
        )
        .await;
    assert_eq!(&r.body[..], &data[990..]);
    let r = s
        .call(
            Method::GET,
            &uri,
            Some(&t),
            vec![],
            &[("range", "bytes=5000-")],
        )
        .await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    let r = s
        .call(
            Method::GET,
            &uri,
            Some(&t),
            vec![],
            &[("if-none-match", &etag)],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    let r = s.call(Method::HEAD, &uri, Some(&t), vec![], &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-length"], "1000");
    assert!(r.body.is_empty());
    let r = s
        .get(&format!("/v1/blobs/{}", Hash::of(b"x").to_hex()), &t)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resumable_upload_with_interruptions() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 256) as u8).collect();
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
    assert!(!st.complete);
    assert_eq!(st.offset, 0);
    let id = st.upload_id.clone();
    let part = |from: usize, to: usize| {
        let range = format!("bytes {}-{}/{}", from, to - 1, data.len());
        (data[from..to].to_vec(), range)
    };
    let (b, range) = part(0, 30_000);
    let r = s
        .call(
            Method::PUT,
            &format!("/v1/uploads/{id}"),
            Some(&t),
            b,
            &[("content-range", &range)],
        )
        .await;
    assert_eq!(r.proto::<pb::UploadState>().offset, 30_000);
    // повтор той же части (клиент не получил ответ) — отказ со смещением
    let (b, range) = part(0, 30_000);
    let r = s
        .call(
            Method::PUT,
            &format!("/v1/uploads/{id}"),
            Some(&t),
            b,
            &[("content-range", &range)],
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.error().code, "offset_mismatch");
    // «телефон заснул»: клиент потерял id и начинает заново — сервер продолжает ту же загрузку
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
    assert_eq!(st.upload_id, id);
    assert_eq!(st.offset, 30_000);
    let cur: pb::UploadState = s.get(&format!("/v1/uploads/{id}"), &t).await.proto();
    assert_eq!(cur.offset, 30_000);
    // commit до конца — отказ
    let r = s
        .call(
            Method::POST,
            &format!("/v1/uploads/{id}/commit"),
            Some(&t),
            vec![],
            &[],
        )
        .await;
    assert_eq!(r.error().code, "upload_incomplete");
    let (b, range) = part(30_000, 100_000);
    s.call(
        Method::PUT,
        &format!("/v1/uploads/{id}"),
        Some(&t),
        b,
        &[("content-range", &range)],
    )
    .await;
    let r = s
        .call(
            Method::POST,
            &format!("/v1/uploads/{id}/commit"),
            Some(&t),
            vec![],
            &[],
        )
        .await;
    assert!(r.proto::<pb::UploadState>().complete);
    let got = s.get(&format!("/v1/blobs/{}", h.to_hex()), &t).await;
    assert_eq!(&got.body[..], &data[..]);
    // повторный старт для готового блоба — сразу complete
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
    assert!(st.complete);
}

#[tokio::test]
async fn upload_commit_detects_hash_mismatch() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let h = Hash::of(b"expected");
    let st: pb::UploadState = s
        .post(
            "/v1/uploads",
            &t,
            &pb::UploadStart {
                hash: h.to_vec(),
                size: 8,
            },
        )
        .await
        .proto();
    let id = st.upload_id;
    s.call(
        Method::PUT,
        &format!("/v1/uploads/{id}"),
        Some(&t),
        b"tampered".to_vec(),
        &[("content-range", "bytes 0-7/8")],
    )
    .await;
    let r = s
        .call(
            Method::POST,
            &format!("/v1/uploads/{id}/commit"),
            Some(&t),
            vec![],
            &[],
        )
        .await;
    assert_eq!(r.error().code, "hash_mismatch");
    assert_eq!(
        s.get(&format!("/v1/uploads/{id}"), &t).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn changes_pagination_and_limits() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    for i in 0..12 {
        s.write_file(&t, &format!("f{i}.md"), 0, format!("{i}").as_bytes())
            .await;
    }
    let c: pb::ChangesResponse = s.get("/v1/changes?since=0&limit=5", &t).await.proto();
    assert_eq!(c.entries.len(), 5);
    assert!(c.has_more);
    assert_eq!(c.next_seq, 5);
    let c2: pb::ChangesResponse = s
        .get(
            &format!("/v1/changes?since={}&limit=5000000", c.next_seq),
            &t,
        )
        .await
        .proto();
    assert_eq!(c2.entries.len(), 7);
    assert!(!c2.has_more);
    let empty = s.changes(&t, 100).await;
    assert!(empty.entries.is_empty());
    assert_eq!(empty.next_seq, 100);
    assert_eq!(
        empty.vault.unwrap().seq,
        12,
        "клиент видит, что его курсор впереди сервера"
    );
    // лимит батча
    let ops: Vec<_> = (0..1001).map(|i| del_op(&format!("x{i}"), 0)).collect();
    let r = s.post("/v1/ops", &t, &pb::OpsRequest { ops }).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn body_limit_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = notesync_server::Config::for_tests(dir.path().to_path_buf());
    config.max_body_size = 1024;
    let s = TestServer::with_config(dir, config, Mode::Serve);
    let t = s.token("notes", "a");
    let big = pb::HashList {
        hashes: vec![vec![1; 32]; 100],
    };
    let r = s.post("/v1/blobs/missing", &t, &big).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn rename_and_occupied_destination() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "a.md", 0, b"A").await;
    s.write_file(&t, "b.md", 0, b"B").await;
    let r = s.ops(&t, vec![ren_op("a.md", "b.md", 1)]).await;
    match &r.results[0].result {
        Some(pb::op_result::Result::Conflict(c)) => {
            assert!(c.at_destination);
            assert_eq!(c.server.as_ref().unwrap().path, Some(p("b.md")));
        }
        o => panic!("{o:?}"),
    }
    let r = s.ops(&t, vec![ren_op("a.md", "c.md", 1)]).await;
    let a = applied(&r.results[0]);
    let ch = s.changes(&t, 2).await;
    assert_eq!(ch.entries[0].path, Some(p("c.md")));
    assert_eq!(ch.entries[0].renamed_from, Some(p("a.md")));
    assert_eq!(ch.entries[0].seq, a.seq);
    assert!(ch.entries[1].deleted);
    // переименование только регистра
    let r = s.ops(&t, vec![ren_op("c.md", "C.md", a.rev)]).await;
    assert!(!applied(&r.results[0]).noop);
}

#[tokio::test]
async fn trash_restore_and_purge() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "keep.md", 0, b"v1").await;
    s.write_file(&t, "gone.md", 0, b"bye").await;
    s.ops(&t, vec![del_op("keep.md", 1), del_op("gone.md", 1)])
        .await;
    let d: pb::DeletedResponse = s.get("/v1/deleted", &t).await.proto();
    assert_eq!(d.items.len(), 2);
    let item = d
        .items
        .iter()
        .find(|i| i.entry.as_ref().unwrap().path == Some(p("keep.md")))
        .unwrap();
    assert!(item.expires_at > item.entry.as_ref().unwrap().deleted_at);
    // восстановление — обычный Put старого хэша с base_rev tombstone'а
    let last = item.last_live.as_ref().unwrap();
    let r = s
        .ops(
            &t,
            vec![put_op(
                "keep.md",
                item.entry.as_ref().unwrap().rev,
                Hash::from_slice(&last.hash).unwrap(),
                last.size,
            )],
        )
        .await;
    assert!(!applied(&r.results[0]).noop);
    // окончательное стирание
    let gone_hash = {
        let d: pb::DeletedResponse = s.get("/v1/deleted", &t).await.proto();
        assert_eq!(d.items.len(), 1);
        d.items[0].last_live.as_ref().unwrap().hash.clone()
    };
    let r: pb::PurgeResult = s
        .call(
            Method::DELETE,
            "/v1/deleted",
            Some(&t),
            pb::PathList {
                paths: vec![p("gone.md")],
            }
            .encode_to_vec(),
            &[],
        )
        .await
        .proto();
    assert_eq!(r.purged, 1);
    let blob = s
        .get(
            &format!(
                "/v1/blobs/{}",
                Hash::from_slice(&gone_hash).unwrap().to_hex()
            ),
            &t,
        )
        .await;
    assert_eq!(blob.status, StatusCode::NOT_FOUND, "блоб освобождён");
    let after = s.changes(&t, 0).await.vault.unwrap();
    assert!(after.purged_seq > 0);
}

#[tokio::test]
async fn history_endpoint() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "h.md", 0, b"1").await;
    s.write_file(&t, "h.md", 1, b"2").await;
    s.write_file(&t, "h.md", 2, b"3").await;
    let q = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(p("h.md").encode_to_vec());
    let h: pb::HistoryResponse = s.get(&format!("/v1/history?path={q}"), &t).await.proto();
    assert_eq!(
        h.revisions.iter().map(|r| r.rev).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    let r = s.get("/v1/history?path=!!!", &t).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn retention_setting() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    let r: pb::Retention = s.get("/v1/retention", &t).await.proto();
    assert_eq!(r.days, 30);
    let r: pb::Retention = s
        .call(
            Method::PUT,
            "/v1/retention",
            Some(&t),
            pb::Retention { days: 7 }.encode_to_vec(),
            &[],
        )
        .await
        .proto();
    assert_eq!(r.days, 7);
    assert_eq!(s.changes(&t, 0).await.vault.unwrap().retention_days, 7);
    let bad = s
        .call(
            Method::PUT,
            "/v1/retention",
            Some(&t),
            pb::Retention { days: 0 }.encode_to_vec(),
            &[],
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vault_isolation() {
    let s = TestServer::new();
    let ta = s.token("alpha", "a");
    let tb = s.token("beta", "b");
    s.write_file(&ta, "secret.md", 0, b"alpha only").await;
    let b = s.changes(&tb, 0).await;
    assert!(b.entries.is_empty());
    let ea = s.changes(&ta, 0).await;
    let h = Hash::from_slice(&ea.entries[0].hash).unwrap();
    let r = s.get(&format!("/v1/blobs/{}", h.to_hex()), &tb).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "блобы vault'ов раздельны");
    let devs: pb::DevicesResponse = s.get("/v1/devices", &tb).await.proto();
    assert_eq!(devs.devices.len(), 1);
    assert_eq!(devs.vault, "beta");
    // отозвать чужое устройство нельзя
    let alpha_dev: pb::DevicesResponse = s.get("/v1/devices", &ta).await.proto();
    let r = s
        .call(
            Method::DELETE,
            &format!("/v1/devices/{}", alpha_dev.self_id),
            Some(&tb),
            vec![],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(s.dir.path().join("vaults/alpha/meta.db").is_file());
    assert!(s.dir.path().join("vaults/beta/meta.db").is_file());
}

#[tokio::test]
async fn join_code_flow() {
    let s = TestServer::new();
    let t = s.token("notes", "laptop");
    let code: pb::JoinCode = s
        .call(
            Method::POST,
            "/v1/join",
            Some(&t),
            pb::JoinCreate {
                name: "phone".into(),
            }
            .encode_to_vec(),
            &[
                ("host", "notes.example.com"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await
        .proto();
    assert_eq!(
        code.url,
        format!("https://notes.example.com/join/{}", code.code)
    );
    // HTML-страница не расходует код
    let page = s
        .send(
            http::Request::get(format!("/join/{}", code.code))
                .header("accept-language", "ru-RU,ru")
                .header("host", "notes.example.com")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8(page.body.to_vec()).unwrap();
    assert!(html.contains("Подключить"));
    assert!(html.contains("obsidian://notesync-connect?server="));
    assert!(!html.contains("<script"));
    // обмен
    let redeem = pb::JoinRedeem {
        code: code.code.clone(),
        name: String::new(),
    };
    let r = s
        .send(
            http::Request::post("/v1/join/redeem")
                .header("x-notesync-proto", "1")
                .body(axum::body::Body::from(redeem.encode_to_vec()))
                .unwrap(),
        )
        .await;
    let tok: pb::JoinToken = r.proto();
    assert_eq!(tok.vault, "notes");
    assert_eq!(tok.device_name, "phone");
    assert_eq!(
        s.get("/v1/changes", &tok.token).await.status,
        StatusCode::OK
    );
    // повторно — нельзя
    let r = s
        .send(
            http::Request::post("/v1/join/redeem")
                .header("x-notesync-proto", "1")
                .body(axum::body::Body::from(redeem.encode_to_vec()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let page = s
        .send(
            http::Request::get(format!("/join/{}", code.code))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_join_code() {
    let s = TestServer::new();
    let t = s.token("notes", "laptop");
    let code: pb::JoinCode = s
        .post("/v1/join", &t, &pb::JoinCreate { name: "x".into() })
        .await
        .proto();
    {
        let pool = s.state.server_pool().unwrap();
        pool.get()
            .unwrap()
            .execute("UPDATE join_codes SET expires_at = 1", [])
            .unwrap();
    }
    let r = s
        .send(
            http::Request::post("/v1/join/redeem")
                .header("x-notesync-proto", "1")
                .body(axum::body::Body::from(
                    pb::JoinRedeem {
                        code: code.code,
                        name: "x".into(),
                    }
                    .encode_to_vec(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wait_wakes_on_change_and_times_out() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    // таймаут
    let start = std::time::Instant::now();
    let st: pb::VaultState = s.get("/v1/wait?since=0&timeout=1", &t).await.proto();
    assert_eq!(st.seq, 0);
    assert!(start.elapsed() >= Duration::from_millis(900));
    // пробуждение по изменению
    let waiter = {
        let app = s.app.clone();
        let t = t.clone();
        tokio::spawn(async move {
            let req = http::Request::get("/v1/wait?since=0&timeout=20")
                .header("authorization", format!("Bearer {t}"))
                .header("x-notesync-proto", "1")
                .body(axum::body::Body::empty())
                .unwrap();
            let start = std::time::Instant::now();
            let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (pb::VaultState::decode(body).unwrap(), start.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    s.write_file(&t, "a.md", 0, b"x").await;
    let (st, took) = waiter.await.unwrap();
    assert_eq!(st.seq, 1);
    assert!(took < Duration::from_secs(5));
    // уже есть изменения — сразу
    let st: pb::VaultState = s.get("/v1/wait?since=0&timeout=20", &t).await.proto();
    assert_eq!(st.seq, 1);
}

#[tokio::test]
async fn wait_returns_immediately_in_cgi_mode() {
    let s = TestServer::with_mode(Mode::Cgi);
    let t = s.token("notes", "a");
    let start = std::time::Instant::now();
    let st: pb::VaultState = s.get("/v1/wait?since=0&timeout=20", &t).await.proto();
    assert_eq!(st.seq, 0);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn vault_key_versioning_and_migration_purge() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.write_file(&t, "plain.md", 0, b"open text").await;
    let k: pb::VaultKeyResponse = s.get("/v1/vaultkey", &t).await.proto();
    assert_eq!(k.version, 0);
    assert!(k.record.is_empty());
    let put = pb::VaultKeyPut {
        record: b"opaque-record".to_vec(),
    };
    let r = s
        .call(
            Method::PUT,
            "/v1/vaultkey",
            Some(&t),
            put.encode_to_vec(),
            &[],
        )
        .await;
    let k: pb::VaultKeyResponse = r.proto();
    assert_eq!(k.version, 1);
    assert_eq!(r.headers["etag"], "\"1\"");
    // без If-Match повторно нельзя
    let r = s
        .call(
            Method::PUT,
            "/v1/vaultkey",
            Some(&t),
            put.encode_to_vec(),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED);
    // смена пароля: If-Match текущей версии
    let put2 = pb::VaultKeyPut {
        record: b"rewrapped".to_vec(),
    };
    let r = s
        .call(
            Method::PUT,
            "/v1/vaultkey",
            Some(&t),
            put2.encode_to_vec(),
            &[("if-match", "\"1\"")],
        )
        .await;
    assert_eq!(r.proto::<pb::VaultKeyResponse>().version, 2);
    // ключ есть, миграции нет: открытые записи запрещены
    let r = s.write_file(&t, "plain2.md", 0, b"more").await;
    match r.result {
        Some(pb::op_result::Result::Rejected(rj)) => {
            assert_eq!(rj.code, "plaintext_in_encrypted_vault")
        }
        o => panic!("{o:?}"),
    }
    // маркер миграции
    let r = s
        .call(
            Method::PUT,
            "/v1/vaultkey/migration",
            Some(&t),
            pb::MigrationMarker {
                key_version: 2,
                ..Default::default()
            }
            .encode_to_vec(),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(s.changes(&t, 0).await.vault.unwrap().migration);
    // зашифрованная копия
    let enc_path = pb::Path {
        segments: vec![vec![0xAB; 32]],
        encrypted: true,
    };
    let (h, _) = s.put_blob(&t, b"ciphertext").await;
    let resp = s
        .ops(
            &t,
            vec![pb::Op {
                kind: Some(pb::op::Kind::Put(pb::Put {
                    path: Some(enc_path),
                    base_rev: 0,
                    hash: h.to_vec(),
                    size: 10,
                    mtime: 0,
                })),
            }],
        )
        .await;
    applied(&resp.results[0]);
    // purge с устаревшим снимком — отказ
    let plain_hash = s.changes(&t, 0).await.entries[0].hash.clone();
    let r = s
        .post(
            "/v1/vaultkey/migration/purge",
            &t,
            &pb::MigrationPurge { max_seq: 0 },
        )
        .await;
    assert_eq!(r.error().code, "plaintext_changed");
    let r: pb::PurgeResult = s
        .post(
            "/v1/vaultkey/migration/purge",
            &t,
            &pb::MigrationPurge { max_seq: 2 },
        )
        .await
        .proto();
    assert_eq!(r.purged, 1);
    let ch = s.changes(&t, 0).await;
    assert_eq!(ch.entries.len(), 1);
    assert!(ch.entries[0].path.as_ref().unwrap().encrypted);
    let blob = s
        .get(
            &format!(
                "/v1/blobs/{}",
                Hash::from_slice(&plain_hash).unwrap().to_hex()
            ),
            &t,
        )
        .await;
    assert_eq!(blob.status, StatusCode::NOT_FOUND, "открытый блоб стёрт");
    let r = s
        .call(
            Method::DELETE,
            "/v1/vaultkey/migration",
            Some(&t),
            vec![],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(!s.changes(&t, 0).await.vault.unwrap().migration);
}

#[tokio::test]
async fn stats_endpoint() {
    let s = TestServer::new();
    let t = s.token("notes", "a");
    s.token("notes", "b");
    s.write_file(&t, "a.md", 0, b"aaaa").await;
    s.write_file(&t, "b.md", 0, b"bb").await;
    s.ops(&t, vec![del_op("b.md", 1)]).await;
    let st: pb::Stats = s.get("/v1/stats", &t).await.proto();
    assert_eq!(st.files, 1);
    assert_eq!(st.deleted, 1);
    assert_eq!(st.seq, 3);
    assert_eq!(st.devices, 2);
    assert_eq!(st.blobs, 2);
    assert!(st.stored_bytes > 0);
}

#[tokio::test]
async fn unknown_route_is_404() {
    let s = TestServer::new();
    let r = s
        .send(
            http::Request::get("/nope")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}
