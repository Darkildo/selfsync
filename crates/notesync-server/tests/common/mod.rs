//! Общая обвязка интеграционных тестов: сервер in-process, запросы через `oneshot`.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode};
use notesync_core::blob;
use notesync_core::hash::Hash;
use notesync_core::path::VaultPath;
use notesync_proto::v1 as pb;
use notesync_server::{AppState, Config, Mode, router};
use prost::Message;
use tower::ServiceExt;

pub struct TestServer {
    pub dir: tempfile::TempDir,
    pub state: AppState,
    pub app: Router,
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn proto<T: Message + Default>(&self) -> T {
        assert!(
            self.status.is_success(),
            "статус {} тело {:?}",
            self.status,
            pb::Error::decode(self.body.clone()).ok()
        );
        T::decode(self.body.clone()).expect("protobuf")
    }

    pub fn error(&self) -> pb::Error {
        pb::Error::decode(self.body.clone()).expect("Error")
    }
}

impl TestServer {
    pub fn new() -> TestServer {
        Self::with_mode(Mode::Serve)
    }

    pub fn with_mode(mode: Mode) -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::for_tests(dir.path().to_path_buf());
        Self::with_config(dir, config, mode)
    }

    pub fn with_config(dir: tempfile::TempDir, config: Config, mode: Mode) -> TestServer {
        let state = AppState::new(config, mode);
        let app = router(state.clone());
        TestServer { dir, state, app }
    }

    pub fn config(&self) -> &Config {
        &self.state.config
    }

    /// Новый токен устройства в vault'е.
    pub fn token(&self, vault: &str, name: &str) -> String {
        let pool = self.state.server_pool().unwrap();
        let c = pool.get().unwrap();
        notesync_server::db::server::add_device(&c, vault, name)
            .unwrap()
            .1
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn call(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Vec<u8>,
        extra: &[(&str, &str)],
    ) -> Resp {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-notesync-proto", "1");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        self.send(b.body(Body::from(body)).unwrap()).await
    }

    pub async fn get(&self, uri: &str, token: &str) -> Resp {
        self.call(Method::GET, uri, Some(token), vec![], &[]).await
    }

    pub async fn post<M: Message>(&self, uri: &str, token: &str, m: &M) -> Resp {
        self.call(Method::POST, uri, Some(token), m.encode_to_vec(), &[])
            .await
    }

    pub async fn put_blob(&self, token: &str, data: &[u8]) -> (Hash, Resp) {
        let h = Hash::of(data);
        let r = self
            .call(
                Method::PUT,
                &format!("/v1/blobs/{}", h.to_hex()),
                Some(token),
                data.to_vec(),
                &[],
            )
            .await;
        (h, r)
    }

    pub async fn ops(&self, token: &str, ops: Vec<pb::Op>) -> pb::OpsResponse {
        self.post("/v1/ops", token, &pb::OpsRequest { ops })
            .await
            .proto()
    }

    pub async fn changes(&self, token: &str, since: u64) -> pb::ChangesResponse {
        self.get(&format!("/v1/changes?since={since}"), token)
            .await
            .proto()
    }

    /// Загружает содержимое как открытый блоб и кладёт его по пути.
    pub async fn write_file(
        &self,
        token: &str,
        path: &str,
        base_rev: u64,
        content: &[u8],
    ) -> pb::OpResult {
        let b = blob::encode(content, None);
        let (h, r) = self.put_blob(token, &b).await;
        assert!(r.status.is_success(), "{}", r.status);
        let mut resp = self
            .ops(token, vec![put_op(path, base_rev, h, b.len() as u64)])
            .await;
        resp.results.remove(0)
    }
}

pub fn p(s: &str) -> pb::Path {
    VaultPath::parse(s).unwrap().to_proto()
}

pub fn put_op(path: &str, base_rev: u64, hash: Hash, size: u64) -> pb::Op {
    pb::Op {
        kind: Some(pb::op::Kind::Put(pb::Put {
            path: Some(p(path)),
            base_rev,
            hash: hash.to_vec(),
            size,
            mtime: 1,
        })),
    }
}

pub fn del_op(path: &str, base_rev: u64) -> pb::Op {
    pb::Op {
        kind: Some(pb::op::Kind::Delete(pb::Delete {
            path: Some(p(path)),
            base_rev,
        })),
    }
}

pub fn ren_op(from: &str, to: &str, base_rev: u64) -> pb::Op {
    pb::Op {
        kind: Some(pb::op::Kind::Rename(pb::Rename {
            from: Some(p(from)),
            to: Some(p(to)),
            base_rev,
        })),
    }
}

pub fn applied(r: &pb::OpResult) -> pb::Applied {
    match &r.result {
        Some(pb::op_result::Result::Applied(a)) => *a,
        other => panic!("ожидался Applied, получено {other:?}"),
    }
}
