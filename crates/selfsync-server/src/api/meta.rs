//! История, корзина, окно хранения, запись ключа и миграция на шифрование.

use axum::Extension;
use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use prost::Message;
use selfsync_core::path::{canonical_encode, validate_proto_path};
use selfsync_proto::v1 as pb;
use serde::Deserialize;

use super::{AuthCtx, Proto};
use crate::db::now_ms;
use crate::db::vault::{self, PurgeOutcome};
use crate::error::{ApiError, ApiResult};
use crate::gc::remove_unreferenced;

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// base64url от protobuf `Path`.
    path: String,
}

fn decode_path_param(s: &str) -> ApiResult<pb::Path> {
    let bytes = URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| ApiError::bad_request("bad_path", "path — base64url от protobuf Path"))?;
    let p = pb::Path::decode(&bytes[..])
        .map_err(|_| ApiError::bad_request("bad_path", "path не разбирается"))?;
    validate_proto_path(&p).map_err(|e| ApiError::bad_request("bad_path", e.to_string()))?;
    Ok(p)
}

pub async fn history(
    Extension(a): Extension<AuthCtx>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Proto<pb::HistoryResponse>> {
    let p = decode_path_param(&q.path)?;
    let key = canonical_encode(&p);
    let revisions = a.vault.db(move |c, _| Ok(vault::history(c, &key)?)).await?;
    Ok(Proto(pb::HistoryResponse {
        path: Some(p),
        revisions,
    }))
}

pub async fn deleted(Extension(a): Extension<AuthCtx>) -> ApiResult<Proto<pb::DeletedResponse>> {
    let items = a.vault.db(|c, _| Ok(vault::deleted(c, now_ms())?)).await?;
    Ok(Proto(pb::DeletedResponse { items }))
}

/// Окончательно стереть выбранные удалённые файлы (вместе с историей и блобами).
pub async fn purge_deleted(
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::PathList>,
) -> ApiResult<Proto<pb::PurgeResult>> {
    let mut keys = Vec::with_capacity(req.paths.len());
    for p in &req.paths {
        validate_proto_path(p).map_err(|e| ApiError::bad_request("bad_path", e.to_string()))?;
        keys.push(canonical_encode(p));
    }
    let purged = a
        .vault
        .db(move |c, v| {
            let (n, hashes) = vault::purge_deleted(c, &keys)?;
            remove_unreferenced(c, &v.blobs, &hashes, None)?;
            crate::db::truncate_wal(c)?;
            Ok(n)
        })
        .await?;
    Ok(Proto(pb::PurgeResult { purged }))
}

pub async fn get_retention(Extension(a): Extension<AuthCtx>) -> ApiResult<Proto<pb::Retention>> {
    let days = a.vault.db(|c, _| Ok(vault::retention_days(c)?)).await?;
    Ok(Proto(pb::Retention { days }))
}

pub async fn put_retention(
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::Retention>,
) -> ApiResult<Proto<pb::Retention>> {
    if !(1..=3650).contains(&req.days) {
        return Err(ApiError::bad_request(
            "bad_retention",
            "окно хранения: 1–3650 дней",
        ));
    }
    let days = req.days;
    a.vault
        .db(move |c, _| {
            vault::set_meta_int(c, "retention_days", i64::from(days))?;
            Ok(())
        })
        .await?;
    Ok(Proto(pb::Retention { days }))
}

fn etag(version: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("\"0\""))
}

fn key_response(r: pb::VaultKeyResponse) -> Response {
    let tag = etag(r.version);
    let mut resp = Proto(r).into_response();
    resp.headers_mut().insert(header::ETAG, tag);
    resp
}

pub async fn get_vault_key(Extension(a): Extension<AuthCtx>) -> ApiResult<Response> {
    let r = a.vault.db(|c, _| Ok(vault::get_vault_key(c)?)).await?;
    Ok(key_response(r))
}

/// `If-Match: "N"` — ожидаемая версия; без заголовка записи быть не должно.
fn if_match(h: &HeaderMap) -> ApiResult<u64> {
    let Some(v) = h.get(header::IF_MATCH) else {
        return Ok(0);
    };
    let s = v
        .to_str()
        .map_err(|_| ApiError::bad_request("bad_if_match", "неверный If-Match"))?;
    s.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .map_err(|_| ApiError::bad_request("bad_if_match", "неверный If-Match"))
}

pub async fn put_vault_key(
    Extension(a): Extension<AuthCtx>,
    headers: HeaderMap,
    Proto(req): Proto<pb::VaultKeyPut>,
) -> ApiResult<Response> {
    if req.record.is_empty() || req.record.len() > 4096 {
        return Err(ApiError::bad_request(
            "bad_record",
            "запись ключа: 1–4096 байт",
        ));
    }
    let expected = if_match(&headers)?;
    let r = a
        .vault
        .db(
            move |c, _| match vault::put_vault_key(c, &req.record, expected)? {
                Some(_) => Ok(Some(vault::get_vault_key(c)?)),
                None => Ok(None),
            },
        )
        .await?;
    match r {
        Some(r) => Ok(key_response(r)),
        None => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "version_mismatch",
            "запись ключа изменилась: перечитайте её",
        )),
    }
}

pub async fn put_migration(
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::MigrationMarker>,
) -> ApiResult<Proto<pb::MigrationMarker>> {
    let marker = pb::MigrationMarker {
        device: a.device_id,
        started_at: now_ms(),
        key_version: req.key_version,
    };
    let m = marker;
    a.vault
        .db(move |c, _| {
            if vault::meta_blob(c, "vault_key")?.is_none() {
                return Err(ApiError::conflict(
                    "no_vault_key",
                    "сначала нужна запись ключа",
                ));
            }
            // Повтор от того же устройства — no-op: маркер не перезаписывается чужим.
            if let Some(existing) = vault::meta_blob(c, "migration")? {
                let old = pb::MigrationMarker::decode(&existing[..]).unwrap_or_default();
                if old.device != m.device {
                    return Err(ApiError::conflict(
                        "migration_in_progress",
                        "миграцию уже ведёт другое устройство",
                    ));
                }
                return Ok(());
            }
            vault::set_migration(c, &m)?;
            Ok(())
        })
        .await?;
    Ok(Proto(marker))
}

pub async fn delete_migration(Extension(a): Extension<AuthCtx>) -> ApiResult<StatusCode> {
    a.vault
        .db(|c, _| {
            vault::clear_migration(c)?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn purge_migration(
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::MigrationPurge>,
) -> ApiResult<Proto<pb::PurgeResult>> {
    let out = a
        .vault
        .db(move |c, v| {
            let out = vault::purge_plaintext(c, req.max_seq)?;
            if let PurgeOutcome::Done { hashes, .. } = &out {
                // Открытые блобы стираются сразу: ждать gc незачем.
                remove_unreferenced(c, &v.blobs, hashes, None)?;
                crate::db::truncate_wal(c)?;
            }
            Ok(out)
        })
        .await?;
    match out {
        PurgeOutcome::Done { purged, .. } => Ok(Proto(pb::PurgeResult { purged })),
        PurgeOutcome::NoMigration => Err(ApiError::conflict("no_migration", "миграция не начата")),
        PurgeOutcome::PlaintextChanged => Err(ApiError::conflict(
            "plaintext_changed",
            "открытые файлы изменились после снимка: доперезалейте их",
        )),
    }
}
