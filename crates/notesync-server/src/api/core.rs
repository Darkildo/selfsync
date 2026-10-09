//! health, stats, changes, wait, ops.

use std::time::Duration;

use axum::Extension;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use notesync_core::hash::Hash;
use notesync_proto::PROTO_VERSION;
use notesync_proto::v1 as pb;
use serde::Deserialize;

use super::{AuthCtx, Proto};
use crate::db::now_ms;
use crate::db::vault::{self, MAX_OPS, OpsCtx};
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Mode};

/// Единственный JSON-ответ: для людей и мониторинга. БД не трогает.
pub async fn health() -> Response {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "proto": PROTO_VERSION,
    }))
    .into_response()
}

pub async fn stats(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
) -> ApiResult<Proto<pb::Stats>> {
    let mut s = a
        .vault
        .db(|c, v| {
            let mut s = vault::stats(c)?;
            let (bytes, n) = v.blobs.total_size()?;
            s.stored_bytes = bytes;
            s.blobs = n;
            Ok(s)
        })
        .await?;
    let name = a.vault.name.clone();
    s.devices = st
        .server_db(move |c| Ok(crate::db::server::count_devices(c, &name)?))
        .await?;
    Ok(Proto(s))
}

#[derive(Deserialize)]
pub struct ChangesQuery {
    since: Option<u64>,
    limit: Option<usize>,
}

pub const DEFAULT_LIMIT: usize = 500;
pub const MAX_LIMIT: usize = 5000;

pub async fn changes(
    Extension(a): Extension<AuthCtx>,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Proto<pb::ChangesResponse>> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let resp = a
        .vault
        .db(move |c, _| {
            // Дельта и состояние — из одного снимка.
            let tx = c.transaction()?;
            let (entries, has_more) = vault::changes(&tx, since, limit)?;
            let state = vault::vault_state(&tx)?;
            tx.commit()?;
            let next_seq = entries.last().map_or(since, |e| e.seq);
            Ok(pb::ChangesResponse {
                entries,
                next_seq,
                has_more,
                vault: Some(state),
            })
        })
        .await?;
    Ok(Proto(resp))
}

#[derive(Deserialize)]
pub struct WaitQuery {
    since: Option<u64>,
    timeout: Option<u64>,
}

/// Верхняя граница long-poll.
pub const MAX_WAIT: Duration = Duration::from_secs(25);
/// Как часто перечитывать `seq` из БД: писать может и другой процесс (CLI, CGI).
const WAIT_POLL: Duration = Duration::from_secs(2);

pub async fn wait(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Query(q): Query<WaitQuery>,
) -> ApiResult<Proto<pb::VaultState>> {
    let since = q.since.unwrap_or(0);
    let timeout = Duration::from_secs(q.timeout.unwrap_or(25)).min(MAX_WAIT);
    let read = || a.vault.db(|c, _| Ok(vault::vault_state(c)?));
    let state = read().await?;
    // В CGI держать процесс открытым бессмысленно: отвечаем сразу.
    if st.mode == Mode::Cgi || state.seq > since || timeout.is_zero() {
        return Ok(Proto(state));
    }
    let mut rx = a.vault.notify.subscribe();
    let mut down = st.shutdown.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            _ = rx.changed() => {}
            () = tokio::time::sleep(WAIT_POLL) => {}
            () = tokio::time::sleep_until(deadline) => break,
            _ = down.changed() => break,
        }
        let s = read().await?;
        if s.seq > since {
            return Ok(Proto(s));
        }
    }
    Ok(Proto(read().await?))
}

pub async fn ops(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::OpsRequest>,
) -> ApiResult<Proto<pb::OpsResponse>> {
    if req.ops.len() > MAX_OPS {
        return Err(ApiError::too_large(format!(
            "не больше {MAX_OPS} операций за запрос"
        )));
    }
    let device = a.device_id;
    let max_blob_size = st.config.max_blob_size;
    let out = a
        .vault
        .db(move |c, v| {
            let blobs = v.blobs.clone();
            let exists = move |h: &Hash| blobs.exists(h);
            let ctx = OpsCtx {
                device,
                now: now_ms(),
                max_blob_size,
                blob_exists: &exists,
            };
            Ok(vault::apply_ops(c, &ctx, &req.ops)?)
        })
        .await?;
    if out.changed {
        a.vault.notify.send_replace(out.state.seq);
    }
    Ok(Proto(pb::OpsResponse {
        results: out.results,
        vault: Some(out.state),
    }))
}
