//! Клиент протокола v1: типизированные вызовы поверх `Action::Http`. Единственное
//! место, где кодируются запросы и разбираются ответы (TS протокол не трогает).

use prost::Message;
use selfsync_proto::v1 as pb;
use selfsync_proto::{CONTENT_TYPE_OCTETS, CONTENT_TYPE_PROTOBUF, HEADER_PROTO, PROTO_VERSION};

use super::ctx::{Ctx, HTTP_TIMEOUT_MS, SyncError, SyncResult, TRANSFER_TIMEOUT_MS};
use super::types::HttpRequest;
use crate::hash::Hash;

fn base_headers() -> Vec<(String, String)> {
    vec![(HEADER_PROTO.to_owned(), PROTO_VERSION.to_string())]
}

fn req(method: &str, path: String, body: Vec<u8>, content_type: Option<&str>) -> HttpRequest {
    let mut headers = base_headers();
    if let Some(ct) = content_type {
        headers.push(("content-type".to_owned(), ct.to_owned()));
    }
    HttpRequest {
        method: method.to_owned(),
        path,
        headers,
        body,
        auth: true,
        timeout_ms: HTTP_TIMEOUT_MS,
    }
}

fn proto_req<M: Message>(method: &str, path: String, m: &M) -> HttpRequest {
    req(method, path, m.encode_to_vec(), Some(CONTENT_TYPE_PROTOBUF))
}

/// Ошибка из ответа сервера.
fn error_of(status: u16, body: &[u8]) -> SyncError {
    let e = pb::Error::decode(body).unwrap_or_default();
    match status {
        401 => SyncError::Unauthorized,
        426 => SyncError::ProtoUnsupported(e.supported_proto),
        s if s >= 500 => SyncError::Server {
            status: s,
            code: e.code,
        },
        s => SyncError::Http {
            status: s,
            code: e.code,
            message: e.message,
        },
    }
}

async fn call<T: Message + Default>(cx: &Ctx, r: HttpRequest) -> SyncResult<T> {
    let (status, _, body) = cx.http(r).await?;
    if (200..300).contains(&status) {
        T::decode(&body[..]).map_err(|e| SyncError::Protocol(e.to_string()))
    } else {
        Err(error_of(status, &body))
    }
}

async fn call_empty(cx: &Ctx, r: HttpRequest) -> SyncResult<u16> {
    let (status, _, body) = cx.http(r).await?;
    if (200..300).contains(&status) {
        Ok(status)
    } else {
        Err(error_of(status, &body))
    }
}

pub async fn changes(cx: &Ctx, since: u64, limit: u32) -> SyncResult<pb::ChangesResponse> {
    call(
        cx,
        req(
            "GET",
            format!("/v1/changes?since={since}&limit={limit}"),
            vec![],
            None,
        ),
    )
    .await
}

pub async fn wait(cx: &Ctx, since: u64, timeout_s: u32) -> SyncResult<pb::VaultState> {
    let mut r = req(
        "GET",
        format!("/v1/wait?since={since}&timeout={timeout_s}"),
        vec![],
        None,
    );
    r.timeout_ms = u64::from(timeout_s) * 1000 + 15_000;
    call(cx, r).await
}

pub async fn ops(cx: &Ctx, ops: Vec<pb::Op>) -> SyncResult<pb::OpsResponse> {
    call(
        cx,
        proto_req("POST", "/v1/ops".into(), &pb::OpsRequest { ops }),
    )
    .await
}

pub async fn blobs_missing(cx: &Ctx, hashes: &[Hash]) -> SyncResult<Vec<Hash>> {
    let mut out = Vec::new();
    for chunk in hashes.chunks(5000) {
        let r: pb::HashList = call(
            cx,
            proto_req(
                "POST",
                "/v1/blobs/missing".into(),
                &pb::HashList {
                    hashes: chunk.iter().map(Hash::to_vec).collect(),
                },
            ),
        )
        .await?;
        out.extend(r.hashes.iter().filter_map(|h| Hash::from_slice(h)));
    }
    Ok(out)
}

pub async fn put_blob(cx: &Ctx, h: &Hash, data: Vec<u8>) -> SyncResult<()> {
    let mut r = req(
        "PUT",
        format!("/v1/blobs/{}", h.to_hex()),
        data,
        Some(CONTENT_TYPE_OCTETS),
    );
    r.timeout_ms = TRANSFER_TIMEOUT_MS;
    call_empty(cx, r).await.map(|_| ())
}

/// Блоб целиком или диапазон `[start, end]` (включительно).
pub async fn get_blob(cx: &Ctx, h: &Hash, range: Option<(u64, u64)>) -> SyncResult<Vec<u8>> {
    let mut r = req("GET", format!("/v1/blobs/{}", h.to_hex()), vec![], None);
    r.timeout_ms = TRANSFER_TIMEOUT_MS;
    if let Some((a, b)) = range {
        r.headers
            .push(("range".to_owned(), format!("bytes={a}-{b}")));
    }
    let (status, _, body) = cx.http(r).await?;
    match status {
        200 | 206 => Ok(body),
        404 => Err(SyncError::BlobGone),
        s => Err(error_of(s, &body)),
    }
}

pub async fn upload_start(cx: &Ctx, h: &Hash, size: u64) -> SyncResult<pb::UploadState> {
    call(
        cx,
        proto_req(
            "POST",
            "/v1/uploads".into(),
            &pb::UploadStart {
                hash: h.to_vec(),
                size,
            },
        ),
    )
    .await
}

pub async fn upload_state(cx: &Ctx, id: &str) -> SyncResult<pb::UploadState> {
    call(cx, req("GET", format!("/v1/uploads/{id}"), vec![], None)).await
}

/// Часть загрузки. `Ok(None)` — сервер ждёт другое смещение.
pub async fn upload_part(
    cx: &Ctx,
    id: &str,
    offset: u64,
    total: u64,
    data: Vec<u8>,
) -> SyncResult<Option<pb::UploadState>> {
    let end = offset + data.len() as u64 - 1;
    let mut r = req(
        "PUT",
        format!("/v1/uploads/{id}"),
        data,
        Some(CONTENT_TYPE_OCTETS),
    );
    r.headers.push((
        "content-range".to_owned(),
        format!("bytes {offset}-{end}/{total}"),
    ));
    r.timeout_ms = TRANSFER_TIMEOUT_MS;
    let (status, _, body) = cx.http(r).await?;
    match status {
        200 => pb::UploadState::decode(&body[..])
            .map(Some)
            .map_err(|e| SyncError::Protocol(e.to_string())),
        409 => Ok(None),
        s => Err(error_of(s, &body)),
    }
}

pub async fn upload_commit(cx: &Ctx, id: &str) -> SyncResult<pb::UploadState> {
    let mut r = req("POST", format!("/v1/uploads/{id}/commit"), vec![], None);
    r.timeout_ms = TRANSFER_TIMEOUT_MS;
    call(cx, r).await
}

pub async fn get_vault_key(cx: &Ctx) -> SyncResult<pb::VaultKeyResponse> {
    call(cx, req("GET", "/v1/vaultkey".into(), vec![], None)).await
}

/// Запись ключа с проверкой версии. `Ok(None)` — версия изменилась (412).
pub async fn put_vault_key(
    cx: &Ctx,
    record: Vec<u8>,
    expected: u64,
) -> SyncResult<Option<pb::VaultKeyResponse>> {
    let mut r = proto_req("PUT", "/v1/vaultkey".into(), &pb::VaultKeyPut { record });
    if expected > 0 {
        r.headers
            .push(("if-match".to_owned(), format!("\"{expected}\"")));
    }
    let (status, _, body) = cx.http(r).await?;
    match status {
        200 => pb::VaultKeyResponse::decode(&body[..])
            .map(Some)
            .map_err(|e| SyncError::Protocol(e.to_string())),
        412 => Ok(None),
        s => Err(error_of(s, &body)),
    }
}

pub async fn put_migration(cx: &Ctx, key_version: u64) -> SyncResult<()> {
    let _: pb::MigrationMarker = call(
        cx,
        proto_req(
            "PUT",
            "/v1/vaultkey/migration".into(),
            &pb::MigrationMarker {
                key_version,
                ..Default::default()
            },
        ),
    )
    .await?;
    Ok(())
}

pub async fn delete_migration(cx: &Ctx) -> SyncResult<()> {
    call_empty(
        cx,
        req("DELETE", "/v1/vaultkey/migration".into(), vec![], None),
    )
    .await
    .map(|_| ())
}

/// Purge открытых записей (сервер заодно снимает маркер миграции). `Ok(false)` —
/// открытые файлы изменились после снимка.
pub async fn purge_plaintext(cx: &Ctx, max_seq: u64) -> SyncResult<bool> {
    let r = proto_req(
        "POST",
        "/v1/vaultkey/migration/purge".into(),
        &pb::MigrationPurge { max_seq },
    );
    let (status, _, body) = cx.http(r).await?;
    match status {
        200 => Ok(true),
        409 => {
            let e = pb::Error::decode(&body[..]).unwrap_or_default();
            match e.code.as_str() {
                "plaintext_changed" => Ok(false),
                // Маркера уже нет: purge прошёл, а ответ потерялся.
                "no_migration" => Ok(true),
                _ => Err(error_of(409, &body)),
            }
        }
        s => Err(error_of(s, &body)),
    }
}

pub async fn history(cx: &Ctx, path: &pb::Path) -> SyncResult<pb::HistoryResponse> {
    let q = base64url(&path.encode_to_vec());
    call(
        cx,
        req("GET", format!("/v1/history?path={q}"), vec![], None),
    )
    .await
}

pub async fn deleted(cx: &Ctx) -> SyncResult<pb::DeletedResponse> {
    call(cx, req("GET", "/v1/deleted".into(), vec![], None)).await
}

pub async fn purge_deleted(cx: &Ctx, paths: Vec<pb::Path>) -> SyncResult<pb::PurgeResult> {
    call(
        cx,
        proto_req("DELETE", "/v1/deleted".into(), &pb::PathList { paths }),
    )
    .await
}

pub async fn devices(cx: &Ctx) -> SyncResult<pb::DevicesResponse> {
    call(cx, req("GET", "/v1/devices".into(), vec![], None)).await
}

pub async fn revoke_device(cx: &Ctx, id: u32) -> SyncResult<()> {
    call_empty(cx, req("DELETE", format!("/v1/devices/{id}"), vec![], None))
        .await
        .map(|_| ())
}

pub async fn join_create(cx: &Ctx, name: String) -> SyncResult<pb::JoinCode> {
    call(
        cx,
        proto_req("POST", "/v1/join".into(), &pb::JoinCreate { name }),
    )
    .await
}

pub async fn join_redeem(cx: &Ctx, code: String, name: String) -> SyncResult<pb::JoinToken> {
    let mut r = proto_req(
        "POST",
        "/v1/join/redeem".into(),
        &pb::JoinRedeem { code, name },
    );
    r.auth = false;
    call(cx, r).await
}

pub async fn get_retention(cx: &Ctx) -> SyncResult<u32> {
    let r: pb::Retention = call(cx, req("GET", "/v1/retention".into(), vec![], None)).await?;
    Ok(r.days)
}

pub async fn set_retention(cx: &Ctx, days: u32) -> SyncResult<u32> {
    let r: pb::Retention = call(
        cx,
        proto_req("PUT", "/v1/retention".into(), &pb::Retention { days }),
    )
    .await?;
    Ok(r.days)
}

pub async fn stats(cx: &Ctx) -> SyncResult<pb::Stats> {
    call(cx, req("GET", "/v1/stats".into(), vec![], None)).await
}

/// base64url без паддинга (для параметра `path`).
pub fn base64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let idx = (n >> (18 - 6 * i)) & 63;
            out.push(char::from(A[idx as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_reference() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }
}
