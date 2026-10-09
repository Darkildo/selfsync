//! Устройства vault'а и подключение новых по одноразовому коду.

use axum::Extension;
use axum::extract::{Path, Request, State};
use http::StatusCode;
use prost::Message;
use selfsync_proto::v1 as pb;

use super::{AuthCtx, Proto, check_proto, public_base};
use crate::db::server;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub async fn list(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
) -> ApiResult<Proto<pb::DevicesResponse>> {
    let vault = a.vault.name.clone();
    let v2 = vault.clone();
    let rows = st
        .server_db(move |c| Ok(server::list_devices(c, Some(&v2))?))
        .await?;
    Ok(Proto(pb::DevicesResponse {
        devices: rows
            .into_iter()
            .map(|d| pb::Device {
                id: d.id,
                name: d.name,
                created_at: d.created_at,
                last_seen: d.last_seen,
                revoked: d.revoked,
            })
            .collect(),
        self_id: a.device_id,
        vault,
    }))
}

/// Отозвать устройство своего vault'а (в том числе себя — выход с устройства).
pub async fn revoke(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Path(id): Path<u32>,
) -> ApiResult<StatusCode> {
    let vault = a.vault.name.clone();
    let ok = st
        .server_db(move |c| Ok(server::revoke_by_id(c, &vault, id)?))
        .await?;
    if ok {
        tracing::info!(device = id, by = a.device_id, "устройство отозвано");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("device_not_found"))
    }
}

/// Одноразовый код подключения нового устройства (живёт 15 минут).
pub async fn join_create(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    headers: http::HeaderMap,
    Proto(req): Proto<pb::JoinCreate>,
) -> ApiResult<Proto<pb::JoinCode>> {
    let name = if req.name.trim().is_empty() {
        "new device".to_owned()
    } else {
        req.name.trim().chars().take(128).collect()
    };
    let vault = a.vault.name.clone();
    let by = a.device_id;
    let (code, expires_at) = st
        .server_db(move |c| Ok(server::create_join_code(c, &vault, &name, Some(by))?))
        .await?;
    let url = format!("{}/join/{code}", public_base(&st, &headers));
    Ok(Proto(pb::JoinCode {
        code,
        url,
        expires_at,
    }))
}

/// Обмен кода на токен. Не требует авторизации; код одноразовый.
pub async fn join_redeem(
    State(st): State<AppState>,
    req: Request,
) -> ApiResult<Proto<pb::JoinToken>> {
    check_proto(req.headers())?;
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|_| ApiError::bad_request("bad_body", "не удалось прочитать тело"))?;
    let r = pb::JoinRedeem::decode(body)
        .map_err(|_| ApiError::bad_request("bad_protobuf", "тело не разбирается"))?;
    let code = r.code.trim().to_ascii_lowercase();
    if code.len() != 16 {
        return Err(ApiError::not_found("code_invalid"));
    }
    let name = r.name.chars().take(128).collect::<String>();
    let res = st
        .server_db(move |c| Ok(server::redeem_join_code(c, &code, &name)?))
        .await?;
    let Some((device_id, token, vault, device_name)) = res else {
        // Неизвестный, истёкший и уже использованный код неотличимы.
        return Err(ApiError::not_found("code_invalid"));
    };
    tracing::info!(device = device_id, "устройство подключено по коду");
    Ok(Proto(pb::JoinToken {
        token,
        vault,
        device_id,
        device_name,
    }))
}
