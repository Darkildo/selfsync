//! HTTP API. Роутер общий для всех режимов запуска: CGI вызывает его через
//! `tower::ServiceExt::oneshot`, socket/serve — через hyper.

mod blobs;
mod core;
mod devices;
mod joinpage;
mod meta;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use prost::Message;
use selfsync_proto::{
    CONTENT_TYPE_PROTOBUF, HEADER_CLIENT, HEADER_PROTO, MINOR_READ_ONLY, PROTO_VERSION,
    parse_proto, proto_header,
};

use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Vault};
use crate::throttle;

/// Устройство, от имени которого выполняется запрос.
#[derive(Clone)]
pub struct AuthCtx {
    pub device_id: u32,
    pub device_name: String,
    pub vault: Arc<Vault>,
    /// Устройство только для чтения (сервер пропускает лишь GET и HEAD).
    pub read_only: bool,
}

/// Id устройства в расширениях ответа — для access-лога.
#[derive(Clone, Copy)]
struct DeviceTag(u32);

pub fn router(state: AppState) -> Router {
    let max_body = usize::try_from(state.config.max_body_size).unwrap_or(usize::MAX);
    let authed = Router::new()
        .route("/v1/stats", get(core::stats))
        .route("/v1/changes", get(core::changes))
        .route("/v1/wait", get(core::wait))
        .route("/v1/ops", post(core::ops))
        .route("/v1/blobs/missing", post(blobs::missing))
        .route(
            "/v1/blobs/{hash}",
            put(blobs::put_blob).get(blobs::get_blob),
        )
        .route("/v1/uploads", post(blobs::upload_start))
        .route(
            "/v1/uploads/{id}",
            get(blobs::upload_state).put(blobs::upload_part),
        )
        .route("/v1/uploads/{id}/commit", post(blobs::upload_commit))
        .route("/v1/history", get(meta::history))
        .route(
            "/v1/deleted",
            get(meta::deleted).delete(meta::purge_deleted),
        )
        .route(
            "/v1/retention",
            get(meta::get_retention).put(meta::put_retention),
        )
        .route(
            "/v1/vaultkey",
            get(meta::get_vault_key).put(meta::put_vault_key),
        )
        .route(
            "/v1/vaultkey/migration",
            put(meta::put_migration).delete(meta::delete_migration),
        )
        .route("/v1/vaultkey/migration/purge", post(meta::purge_migration))
        .route("/v1/devices", get(devices::list))
        .route("/v1/devices/{id}", delete(devices::revoke))
        .route("/v1/join", post(devices::join_create))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .route("/v1/health", get(core::health))
        .route("/v1/join/redeem", post(devices::join_redeem))
        .route("/join/{code}", get(joinpage::page))
        .merge(authed)
        .fallback(|| async { ApiError::not_found("no_route") })
        .layer(DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn_with_state(state.clone(), access))
        .with_state(state)
}

/// Проверка `X-Selfsync-Proto`: нет заголовка — 400, другой мажор — 426.
/// Возвращает минорную версию клиента (0.1.x шлёт `1` — это `1.0`).
pub fn check_proto(headers: &HeaderMap) -> ApiResult<u32> {
    let Some(v) = headers.get(HEADER_PROTO) else {
        return Err(ApiError::bad_request(
            "proto_header_missing",
            "нужен заголовок X-Selfsync-Proto",
        ));
    };
    match v.to_str().ok().and_then(parse_proto) {
        Some((major, minor)) if major == PROTO_VERSION => Ok(minor),
        _ => Err(ApiError::new(
            StatusCode::UPGRADE_REQUIRED,
            "proto_unsupported",
            format!("сервер поддерживает протокол версии {PROTO_VERSION}"),
        )),
    }
}

/// Клиент старее 1.1 не знает устройств только для чтения: с таким токеном он
/// пытался бы отправлять правки и получал бы непонятные ему 403. Отказ 426 — его
/// клиент показывает как «обновите плагин или сервер» и больше не повторяет.
pub fn client_outdated_for_read_only(minor: u32) -> Option<ApiError> {
    (minor < MINOR_READ_ONLY).then(|| {
        ApiError::new(
            StatusCode::UPGRADE_REQUIRED,
            "client_outdated",
            "устройство подключено только для чтения: нужен плагин или selfsync-cli 0.2.1 или новее",
        )
    })
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_owned())
}

async fn auth(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let minor = match check_proto(req.headers()) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    // Запрос без токена ничего не перебирает и в БД не ходит: не считается.
    let Some(token) = bearer(req.headers()) else {
        return ApiError::unauthorized().into_response();
    };
    let ip = throttle::client_ip(&st.config, req.headers(), req.extensions());
    let keys = throttle::auth_keys(&st.config.rate, ip);
    let client = req
        .headers()
        .get(HEADER_CLIENT)
        .and_then(|v| v.to_str().ok())
        .map(crate::db::server::client_label)
        .filter(|s| !s.is_empty());
    let found = st
        .server_db(move |c| {
            if let Some(secs) = throttle::check_now(c, &keys)? {
                return Err(ApiError::too_many(secs));
            }
            let dev = crate::db::server::authenticate(c, &token)?;
            match &dev {
                None => throttle::fail_now(c, &keys)?,
                Some(d) => {
                    if let Some(cl) = client.as_deref()
                        && cl != d.client
                    {
                        crate::db::server::set_client(c, d.id, cl)?;
                    }
                }
            }
            Ok(dev)
        })
        .await;
    let dev = match found {
        Ok(Some(d)) => d,
        Ok(None) => return ApiError::unauthorized().into_response(),
        Err(e) => return e.into_response(),
    };
    if dev.read_only
        && let Some(e) = client_outdated_for_read_only(minor)
    {
        let mut resp = e.into_response();
        resp.extensions_mut().insert(DeviceTag(dev.id));
        return resp;
    }
    // Правило одно и проверяемое: устройству только для чтения — только GET и HEAD.
    // Отсюда же: ни записи, ни ключа, ни отзыва, ни новых кодов подключения.
    if dev.read_only && !matches!(*req.method(), http::Method::GET | http::Method::HEAD) {
        let mut resp = ApiError::new(
            StatusCode::FORBIDDEN,
            "read_only",
            "устройство подключено только для чтения",
        )
        .into_response();
        resp.extensions_mut().insert(DeviceTag(dev.id));
        return resp;
    }
    let vault = match st.vault(&dev.vault) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let id = dev.id;
    req.extensions_mut().insert(AuthCtx {
        device_id: dev.id,
        device_name: dev.name,
        vault,
        read_only: dev.read_only,
    });
    let mut resp = next.run(req).await;
    resp.extensions_mut().insert(DeviceTag(id));
    resp
}

/// Снимает счётчик запросов в обработке, даже если обработчик упал.
struct InFlight<'a>(&'a AppState);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.activity.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.activity.touch();
    }
}

/// Access-лог и учёт активности. Ожидающий `/v1/wait` не считается запросом в
/// обработке (иначе процесс с long-poll клиентом никогда не выйдет по простою),
/// но его начало и конец — это активность.
async fn access(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let is_wait = path == "/v1/wait";
    st.activity.touch();
    let guard = if is_wait {
        None
    } else {
        st.activity.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(InFlight(&st))
    };
    let mut resp = next.run(req).await;
    drop(guard);
    // Версия протокола в каждом ответе, включая ошибки и health: по ней клиент
    // узнаёт, что умеет сервер (ответ без заголовка — сервер 0.1.x, протокол 1.0).
    if let Ok(v) = HeaderValue::from_str(&proto_header()) {
        resp.headers_mut().insert(HEADER_PROTO, v);
    }
    st.activity.touch();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let device = resp.extensions().get::<DeviceTag>().map(|d| d.0);
    let status = resp.status().as_u16();
    // Пути API не содержат имён файлов (они в теле), токены и пароли не логируются.
    if path == "/v1/health" {
        tracing::debug!(%method, %path, status, ms, "request");
    } else {
        tracing::info!(%method, %path, status, ms, device, "request");
    }
    if !st.first_request_done.swap(true, Ordering::SeqCst) {
        crate::sweep::spawn_after_first_request(&st);
    }
    resp
}

/// Тело или ответ в protobuf.
pub struct Proto<T>(pub T);

impl<S, T> FromRequest<S> for Proto<T>
where
    S: Send + Sync,
    T: Message + Default,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::too_large("тело запроса больше лимита")
            } else {
                ApiError::bad_request("bad_body", "не удалось прочитать тело")
            }
        })?;
        T::decode(bytes)
            .map(Proto)
            .map_err(|_| ApiError::bad_request("bad_protobuf", "тело не разбирается как protobuf"))
    }
}

impl<T: Message> IntoResponse for Proto<T> {
    fn into_response(self) -> Response {
        let mut resp = self.0.encode_to_vec().into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(CONTENT_TYPE_PROTOBUF),
        );
        resp
    }
}

/// Внешний адрес сервера для ссылок подключения.
pub fn public_base(st: &AppState, headers: &HeaderMap) -> String {
    if let Some(u) = &st.config.public_url {
        return u.clone();
    }
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
}
