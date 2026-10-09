//! CGI/1.1 (RFC 3875): процесс на каждый запрос.
//!
//! Запрос собирается из переменных окружения и stdin (ровно `CONTENT_LENGTH` байт,
//! потоком), прогоняется через общий `axum::Router` (`oneshot`), ответ пишется в
//! stdout в CGI-формате, тоже потоком. Логи — только в stderr.

use std::collections::HashMap;

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderName, HeaderValue, Method, Request, Response};
use http_body_util::BodyExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use tower::ServiceExt;

use crate::api::router;
use crate::state::AppState;

/// Включён ли CGI автоматически (fcgiwrap запускает бинарь без аргументов).
pub fn detected() -> bool {
    std::env::var("GATEWAY_INTERFACE").is_ok_and(|v| v.starts_with("CGI/"))
}

/// Путь запроса: `PATH_INFO`, затем `SCRIPT_NAME + PATH_INFO`, затем `REQUEST_URI`.
/// Берётся первый, похожий на маршрут selfsync, — так работают и caddy-cgi (где
/// `SCRIPT_NAME` может «съесть» префикс), и fcgiwrap.
fn request_path(env: &HashMap<String, String>) -> String {
    let get = |k: &str| env.get(k).map(String::as_str).unwrap_or("");
    let path_info = get("PATH_INFO");
    let script = get("SCRIPT_NAME");
    let uri_path = get("REQUEST_URI").split('?').next().unwrap_or("");
    let joined = format!("{}{}", script.trim_end_matches('/'), path_info);
    let looks_ok = |p: &str| p.starts_with("/v1/") || p.starts_with("/join/");
    for cand in [path_info, joined.as_str(), uri_path] {
        if looks_ok(cand) {
            return cand.to_owned();
        }
    }
    if !path_info.is_empty() {
        path_info.to_owned()
    } else if !uri_path.is_empty() {
        uri_path.to_owned()
    } else {
        "/".to_owned()
    }
}

/// Собирает `http::Request` из CGI-окружения. Тело — поток из `stdin`.
pub fn build_request<R>(env: &HashMap<String, String>, stdin: R) -> anyhow::Result<Request<Body>>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let method = Method::from_bytes(
        env.get("REQUEST_METHOD")
            .map_or("GET", String::as_str)
            .as_bytes(),
    )?;
    let path = request_path(env);
    let query = env.get("QUERY_STRING").map(String::as_str).unwrap_or("");
    let uri = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    let mut b = Request::builder().method(method).uri(uri);
    let headers = b
        .headers_mut()
        .ok_or_else(|| anyhow::anyhow!("request builder"))?;
    for (k, v) in env {
        let name = if let Some(rest) = k.strip_prefix("HTTP_") {
            rest.replace('_', "-").to_ascii_lowercase()
        } else if k == "CONTENT_TYPE" {
            "content-type".to_owned()
        } else if k == "CONTENT_LENGTH" {
            "content-length".to_owned()
        } else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        if let (Ok(n), Ok(val)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            headers.append(n, val);
        }
    }
    // Схема для ссылок подключения: веб-сервер сообщает её отдельно.
    if !headers.contains_key("x-forwarded-proto") {
        let https = env
            .get("HTTPS")
            .is_some_and(|v| v.eq_ignore_ascii_case("on") || v == "1")
            || env.get("REQUEST_SCHEME").is_some_and(|v| v == "https");
        headers.insert(
            "x-forwarded-proto",
            HeaderValue::from_static(if https { "https" } else { "http" }),
        );
    }
    let len: u64 = env
        .get("CONTENT_LENGTH")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let body = if len == 0 {
        Body::empty()
    } else {
        // Ровно CONTENT_LENGTH байт, потоком: большие тела не читаются в память.
        Body::from_stream(ReaderStream::with_capacity(stdin.take(len), 64 * 1024))
    };
    Ok(b.body(body)?)
}

/// Пишет ответ в CGI-формате: `Status:`, заголовки, пустая строка, тело потоком.
pub async fn write_response<W>(resp: Response<Body>, out: &mut W) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let status = resp.status();
    let mut head = format!(
        "Status: {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (k, v) in resp.headers() {
        if let Ok(v) = v.to_str() {
            head.push_str(k.as_str());
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes()).await?;
    let mut stream = resp.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk?;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    Ok(())
}

/// Обработка одного запроса. Отдельно от `main` для тестов.
pub async fn handle<R, W>(
    state: AppState,
    env: HashMap<String, String>,
    stdin: R,
    out: &mut W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Unpin,
{
    let req = build_request(&env, stdin)?;
    let resp = router(state).oneshot(req).await?;
    let (parts, body) = resp.into_parts();
    let resp = Response::from_parts(parts, Body::new(body.map_err(axum::Error::new)));
    write_response(resp, out).await
}

/// Точка входа режима: current_thread runtime, один запрос, выход.
pub fn run(state: AppState) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let env: HashMap<String, String> = std::env::vars().collect();
        let mut out = tokio::io::stdout();
        handle(state, env, tokio::io::stdin(), &mut out).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn path_resolution() {
        assert_eq!(
            request_path(&env(&[("PATH_INFO", "/v1/health")])),
            "/v1/health"
        );
        assert_eq!(
            request_path(&env(&[("SCRIPT_NAME", "/v1"), ("PATH_INFO", "/changes")])),
            "/v1/changes"
        );
        assert_eq!(
            request_path(&env(&[
                ("SCRIPT_NAME", "/cgi-bin/selfsync"),
                ("REQUEST_URI", "/v1/ops?x=1")
            ])),
            "/v1/ops"
        );
        assert_eq!(request_path(&env(&[])), "/");
    }

    #[tokio::test]
    async fn builds_request_with_headers_and_body() {
        let e = env(&[
            ("REQUEST_METHOD", "POST"),
            ("PATH_INFO", "/v1/ops"),
            ("QUERY_STRING", "a=1"),
            ("CONTENT_LENGTH", "5"),
            ("CONTENT_TYPE", "application/x-protobuf"),
            ("HTTP_AUTHORIZATION", "Bearer ns_x"),
            ("HTTP_X_SELFSYNC_PROTO", "1"),
            ("HTTPS", "on"),
        ]);
        let stdin: &'static [u8] = b"helloEXTRA";
        let req = build_request(&e, stdin).unwrap();
        assert_eq!(req.method(), Method::POST);
        assert_eq!(req.uri(), "/v1/ops?a=1");
        assert_eq!(req.headers()["authorization"], "Bearer ns_x");
        assert_eq!(req.headers()["x-selfsync-proto"], "1");
        assert_eq!(req.headers()["x-forwarded-proto"], "https");
        let body = axum::body::to_bytes(req.into_body(), 100).await.unwrap();
        assert_eq!(&body[..], b"hello");
    }
}
