//! Устройства vault'а и подключение новых по одноразовому коду.

use axum::Extension;
use axum::extract::{Path, Request, State};
use http::StatusCode;
use prost::Message;
use selfsync_proto::v1 as pb;

use super::{AuthCtx, Proto, check_proto, client_outdated_for_read_only, public_base};
use crate::db::server;
use crate::error::{ApiError, ApiResult};
use crate::quota;
use crate::state::AppState;
use crate::throttle;

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
                read_only: d.read_only,
                client: d.client,
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
    let read_only = req.read_only;
    let commit = match req.commit.len() {
        0 => None,
        32 => Some(req.commit),
        _ => {
            return Err(ApiError::bad_request(
                "bad_commit",
                "commit — 32 байта sha256",
            ));
        }
    };
    let defaults = st.config.quota.defaults;
    let limit = a
        .vault
        .db(move |c, _| Ok(quota::limits(c, &defaults)?.devices))
        .await?;
    let (code, expires_at) = st
        .server_db(move |c| {
            // Ссылка, по которой подключиться не выйдет, никому не нужна.
            server::check_device_limit(c, &vault, limit)?;
            let (code, expires_at) =
                server::create_join_code(c, &vault, &name, Some(by), read_only)?;
            if let Some(commit) = &commit {
                server::offer_handoff(c, &code, commit)?;
            }
            Ok((code, expires_at))
        })
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
    let minor = check_proto(req.headers())?;
    let ip = throttle::client_ip(&st.config, req.headers(), req.extensions());
    let keys = throttle::redeem_keys(&st.config.rate, ip);
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|_| ApiError::bad_request("bad_body", "не удалось прочитать тело"))?;
    let r = pb::JoinRedeem::decode(body)
        .map_err(|_| ApiError::bad_request("bad_protobuf", "тело не разбирается"))?;
    let code = r.code.trim().to_ascii_lowercase();
    let name = r.name.chars().take(128).collect::<String>();
    let st2 = st.clone();
    let res = st
        .server_db(move |c| {
            if let Some(secs) = throttle::check_now(c, &keys)? {
                return Err(ApiError::too_many(secs));
            }
            // Клиенту, который не знает режима, код «только для чтения» не отдаётся
            // и не гасится: обновив плагин, по нему же можно подключиться.
            if code.len() == 16
                && server::join_code_read_only(c, &code)? == Some(true)
                && let Some(e) = client_outdated_for_read_only(minor)
            {
                return Err(e);
            }
            let res = if code.len() == 16 {
                server::redeem_join_code(c, &code, &name, &|v| quota::device_limit(&st2, v))?
            } else {
                None
            };
            if res.is_none() {
                throttle::fail_now(c, &keys)?;
            }
            Ok(res)
        })
        .await?;
    let Some((device_id, token, vault, device_name)) = res else {
        // Неизвестный, истёкший и уже использованный код неотличимы.
        return Err(ApiError::not_found("code_invalid"));
    };
    tracing::info!(device = device_id, "устройство подключено по коду");
    let handoff_commit = st
        .server_db(move |c| Ok(server::joiner_commit(c, device_id)?))
        .await?
        .unwrap_or_default();
    Ok(Proto(pb::JoinToken {
        token,
        vault,
        device_id,
        device_name,
        handoff_commit,
    }))
}

/// Ключ обмена: несжатая точка P-256 на кривой.
fn handoff_pk(pk: &[u8]) -> ApiResult<()> {
    if selfsync_core::handoff::valid_public(pk) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "bad_handoff_key",
            "ключ обмена — несжатая точка P-256 (65 байт)",
        ))
    }
}

/// Передача ключа по коду, шаг источника (устройство, создавшее код): опрос,
/// раскрытие своего ключа, обёртка мастер-ключа. Сервер ничего из этого прочитать
/// не может: он только хранит и отдаёт.
pub async fn handoff_source(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::JoinHandoffSource>,
) -> ApiResult<Proto<pb::JoinHandoffSourceState>> {
    if !req.pk.is_empty() {
        handoff_pk(&req.pk)?;
    }
    if !req.wrapped.is_empty() && !(28..=256).contains(&req.wrapped.len()) {
        return Err(ApiError::bad_request(
            "bad_wrapped",
            "обёртка — от 28 до 256 байт",
        ));
    }
    let code = req.code.trim().replace('-', "").to_ascii_lowercase();
    let vault = a.vault.name.clone();
    let device = a.device_id;
    let pk = (!req.pk.is_empty()).then_some(req.pk);
    let wrapped = (!req.wrapped.is_empty()).then_some(req.wrapped);
    let h = st
        .server_db(move |c| {
            Ok(server::owner_handoff(
                c,
                &vault,
                device,
                &code,
                pk.as_deref(),
                wrapped.as_deref(),
            )?)
        })
        .await?;
    let h = match h {
        Ok(h) => h,
        Err(server::HandoffRefusal::NotFound) => return Err(ApiError::not_found("code_invalid")),
        Err(server::HandoffRefusal::BadCommit) => {
            return Err(ApiError::bad_request(
                "bad_commit",
                "ключ не совпал с обязательством кода",
            ));
        }
    };
    let delivered = h.wrapped.is_some();
    if delivered {
        tracing::info!(device, "ключ vault'а передан по коду");
    }
    Ok(Proto(pb::JoinHandoffSourceState {
        name: h.name,
        pk: h.joiner_pk.unwrap_or_default(),
        mac: h.joiner_mac.unwrap_or_default(),
        delivered,
    }))
}

/// Шаг нового устройства: его ключ обмена (и MAC из QR). Отправляется после
/// получения commit, чтобы сервер не мог подобрать свой ключ источника под цифры.
pub async fn handoff_joiner_put(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::JoinHandoffJoiner>,
) -> ApiResult<StatusCode> {
    handoff_pk(&req.pk)?;
    if !req.mac.is_empty() && req.mac.len() != 32 {
        return Err(ApiError::bad_request("bad_mac", "MAC — 32 байта"));
    }
    let device = a.device_id;
    let ok = st
        .server_db(move |c| {
            let mac = (!req.mac.is_empty()).then_some(req.mac.as_slice());
            Ok(server::joiner_handoff_put(c, device, &req.pk, mac)?)
        })
        .await?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no_handoff"))
    }
}

/// Шаг нового устройства: опрос ключа источника и обёртки.
pub async fn handoff_joiner_get(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
) -> ApiResult<Proto<pb::JoinHandoffJoinerState>> {
    let device = a.device_id;
    let h = st
        .server_db(move |c| Ok(server::joiner_handoff_get(c, device)?))
        .await?
        .ok_or_else(|| ApiError::not_found("no_handoff"))?;
    Ok(Proto(pb::JoinHandoffJoinerState {
        pk: h.owner_pk.unwrap_or_default(),
        wrapped: h.wrapped.unwrap_or_default(),
    }))
}
