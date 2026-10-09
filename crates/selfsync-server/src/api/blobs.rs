//! Блобы и resumable-загрузки.

use std::io::SeekFrom;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use rusqlite::{OptionalExtension, params};
use selfsync_core::hash::Hash;
use selfsync_proto::CONTENT_TYPE_OCTETS;
use selfsync_proto::v1 as pb;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use super::{AuthCtx, Proto};
use crate::blobs::{AppendOutcome, BlobStore, StreamError, WriteOutcome};
use crate::config::SINGLE_PUT_LIMIT;
use crate::db::{i, now_ms, u};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

fn parse_hash(s: &str) -> ApiResult<Hash> {
    Hash::from_hex(s).ok_or_else(|| ApiError::bad_request("bad_hash", "хэш — 64 hex-символа"))
}

fn content_length(h: &HeaderMap) -> Option<u64> {
    h.get(header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

pub async fn missing(
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::HashList>,
) -> ApiResult<Proto<pb::HashList>> {
    if req.hashes.len() > 10_000 {
        return Err(ApiError::too_large("не больше 10000 хэшей за запрос"));
    }
    let store = a.vault.blobs.clone();
    let hashes = tokio::task::spawn_blocking(move || {
        req.hashes
            .into_iter()
            .filter(|h| Hash::from_slice(h).is_none_or(|hh| !store.exists(&hh)))
            .collect::<Vec<_>>()
    })
    .await?;
    Ok(Proto(pb::HashList { hashes }))
}

fn stream_error(e: StreamError) -> ApiError {
    match e {
        StreamError::Io(e) => ApiError::internal(e),
        StreamError::Body => ApiError::bad_request("body_aborted", "тело запроса оборвалось"),
    }
}

/// Однократная загрузка до 8 МиБ: 201 — создан, 200 — уже был.
pub async fn put_blob(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Path(hash): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let h = parse_hash(&hash)?;
    let limit = SINGLE_PUT_LIMIT.min(st.config.max_blob_size);
    if a.vault.blobs.exists(&h) {
        return Ok(StatusCode::OK.into_response());
    }
    if content_length(req.headers()).is_some_and(|n| n > limit) {
        return Err(ApiError::too_large(
            "больше 8 МиБ — используйте /v1/uploads",
        ));
    }
    let body = req.into_body().into_data_stream();
    match a
        .vault
        .blobs
        .put_stream(&h, body, limit)
        .await
        .map_err(stream_error)?
    {
        WriteOutcome::Stored { created: true } => Ok(StatusCode::CREATED.into_response()),
        WriteOutcome::Stored { created: false } => Ok(StatusCode::OK.into_response()),
        WriteOutcome::HashMismatch => Err(ApiError::bad_request(
            "hash_mismatch",
            "содержимое не совпадает с хэшем",
        )),
        WriteOutcome::TooLarge => Err(ApiError::too_large(
            "больше 8 МиБ — используйте /v1/uploads",
        )),
    }
}

/// Разбор `Range: bytes=…` (один диапазон). `Ok(None)` — заголовка нет.
fn parse_range(headers: &HeaderMap, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(v) = headers.get(header::RANGE) else {
        return Ok(None);
    };
    let s = v.to_str().map_err(|_| ())?;
    let spec = s.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        (size.saturating_sub(n), size.saturating_sub(1))
    } else {
        let start: u64 = a.parse().map_err(|_| ())?;
        let end = if b.is_empty() {
            size.saturating_sub(1)
        } else {
            b.parse::<u64>()
                .map_err(|_| ())?
                .min(size.saturating_sub(1))
        };
        (start, end)
    };
    if start >= size || end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// Сырые байты блоба с `Range`, `ETag` и `If-None-Match`. HEAD обслуживается
/// этим же обработчиком (axum отбрасывает тело).
pub async fn get_blob(
    Extension(a): Extension<AuthCtx>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let h = parse_hash(&hash)?;
    let path = a.vault.blobs.path_of(&h);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found("blob_not_found"));
        }
        Err(e) => return Err(e.into()),
    };
    let size = file.metadata().await?.len();
    let etag = format!("\"{}\"", h.to_hex());
    let etag_hv = HeaderValue::from_str(&etag).map_err(ApiError::internal)?;
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && inm.split(',').any(|t| t.trim() == etag || t.trim() == "*")
    {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, etag_hv);
        return Ok(r);
    }
    let range = match parse_range(&headers, size) {
        Ok(r) => r,
        Err(()) => {
            let mut r = ApiError::new(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "bad_range",
                "неверный Range",
            )
            .into_response();
            r.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).map_err(ApiError::internal)?,
            );
            return Ok(r);
        }
    };
    let (status, start, len) = match range {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
        None => (StatusCode::OK, 0, size),
    };
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }
    let body = Body::from_stream(ReaderStream::with_capacity(file.take(len), 64 * 1024));
    let mut r = Response::new(body);
    *r.status_mut() = status;
    let hm = r.headers_mut();
    hm.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_OCTETS),
    );
    hm.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    hm.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    hm.insert(header::ETAG, etag_hv);
    // Блобы неизменяемы.
    hm.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        hm.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len - 1))
                .map_err(ApiError::internal)?,
        );
    }
    Ok(r)
}

fn valid_upload_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn new_upload_id() -> ApiResult<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn file_len(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Начать загрузку. Если блоб уже есть — сразу «готово»; если есть незавершённая
/// загрузка того же блоба — продолжить её (клиент мог потерять upload_id).
pub async fn upload_start(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Proto(req): Proto<pb::UploadStart>,
) -> ApiResult<Proto<pb::UploadState>> {
    let h = Hash::from_slice(&req.hash)
        .ok_or_else(|| ApiError::bad_request("bad_hash", "хэш должен быть 32 байта"))?;
    if req.size > st.config.max_blob_size {
        return Err(ApiError::too_large("файл больше лимита сервера"));
    }
    if a.vault.blobs.exists(&h) {
        return Ok(Proto(pb::UploadState {
            upload_id: String::new(),
            offset: req.size,
            size: req.size,
            complete: true,
        }));
    }
    let size = req.size;
    let state = a
        .vault
        .db(move |c, v| {
            let existing: Option<String> = c
                .query_row(
                    "SELECT id FROM uploads WHERE hash = ?1 AND size = ?2 ORDER BY created_at DESC LIMIT 1",
                    params![h.to_vec(), i(size)],
                    |r| r.get(0),
                )
                .optional()?;
            let id = match existing {
                Some(id) if v.blobs.upload_path(&id).is_file() => id,
                _ => {
                    let id = new_upload_id()?;
                    std::fs::write(v.blobs.upload_path(&id), b"")?;
                    let now = now_ms();
                    c.execute(
                        "INSERT INTO uploads (id, hash, size, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                        params![id, h.to_vec(), i(size), now],
                    )?;
                    id
                }
            };
            let offset = file_len(&v.blobs.upload_path(&id));
            Ok(pb::UploadState {
                upload_id: id,
                offset,
                size,
                complete: false,
            })
        })
        .await?;
    Ok(Proto(state))
}

async fn load_upload(a: &AuthCtx, id: &str) -> ApiResult<(Hash, u64)> {
    if !valid_upload_id(id) {
        return Err(ApiError::not_found("upload_not_found"));
    }
    let id = id.to_owned();
    let row: Option<(Vec<u8>, i64)> = a
        .vault
        .db(move |c, _| {
            Ok(c.query_row(
                "SELECT hash, size FROM uploads WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await?;
    let (h, size) = row.ok_or_else(|| ApiError::not_found("upload_not_found"))?;
    let h = Hash::from_slice(&h).ok_or_else(|| ApiError::internal("битый хэш загрузки"))?;
    Ok((h, u(size)))
}

pub async fn upload_state(
    Extension(a): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<Proto<pb::UploadState>> {
    let (_, size) = load_upload(&a, &id).await?;
    let offset = file_len(&a.vault.blobs.upload_path(&id));
    Ok(Proto(pb::UploadState {
        upload_id: id,
        offset,
        size,
        complete: false,
    }))
}

/// `Content-Range: bytes start-end/total`.
fn parse_content_range(h: &HeaderMap) -> Option<(u64, u64, u64)> {
    let s = h.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = s.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    Some((
        a.trim().parse().ok()?,
        b.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

/// Часть загрузки. Принимаются только части, продолжающие с текущего смещения.
pub async fn upload_part(
    State(st): State<AppState>,
    Extension(a): Extension<AuthCtx>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Proto<pb::UploadState>> {
    let (_, size) = load_upload(&a, &id).await?;
    let (start, end, total) = parse_content_range(req.headers()).ok_or_else(|| {
        ApiError::bad_request("bad_content_range", "нужен Content-Range: bytes a-b/total")
    })?;
    if total != size || end < start || end >= size {
        return Err(ApiError::bad_request(
            "bad_content_range",
            "диапазон не соответствует загрузке",
        ));
    }
    let part_len = end - start + 1;
    if part_len > st.config.max_body_size {
        return Err(ApiError::too_large("часть больше SELFSYNC_MAX_BODY_SIZE"));
    }
    let body = req.into_body().into_data_stream();
    let outcome = a
        .vault
        .blobs
        .append_part(&id, start, body, size, part_len)
        .await
        .map_err(stream_error)?;
    match outcome {
        AppendOutcome::Appended { offset } => {
            let idc = id.clone();
            a.vault
                .db(move |c, _| {
                    c.execute(
                        "UPDATE uploads SET updated_at = ?1 WHERE id = ?2",
                        params![now_ms(), idc],
                    )?;
                    Ok(())
                })
                .await?;
            Ok(Proto(pb::UploadState {
                upload_id: id,
                offset,
                size,
                complete: false,
            }))
        }
        AppendOutcome::OffsetMismatch { current } => Err(ApiError::conflict(
            "offset_mismatch",
            format!("текущее смещение {current}"),
        )),
        AppendOutcome::TooLarge { .. } => {
            Err(ApiError::too_large("часть выходит за размер загрузки"))
        }
        AppendOutcome::Busy => Err(ApiError::conflict(
            "upload_busy",
            "загрузка уже пишется другим запросом",
        )),
    }
}

/// Проверка хэша и атомарный перенос в `blobs/`.
pub async fn upload_commit(
    Extension(a): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<Proto<pb::UploadState>> {
    let (h, size) = load_upload(&a, &id).await?;
    let path = a.vault.blobs.upload_path(&id);
    let remove_row = |a: &AuthCtx, id: String| {
        let v = a.vault.clone();
        async move {
            v.db(move |c, _| {
                c.execute("DELETE FROM uploads WHERE id = ?1", params![id])?;
                Ok(())
            })
            .await
        }
    };
    if a.vault.blobs.exists(&h) {
        let _ = tokio::fs::remove_file(&path).await;
        remove_row(&a, id).await?;
        return Ok(Proto(pb::UploadState {
            upload_id: String::new(),
            offset: size,
            size,
            complete: true,
        }));
    }
    let (actual, len) = BlobStore::hash_file(&path).await?;
    if len != size {
        return Err(ApiError::conflict(
            "upload_incomplete",
            format!("принято {len} из {size} байт"),
        ));
    }
    if actual != h {
        let _ = tokio::fs::remove_file(&path).await;
        remove_row(&a, id).await?;
        return Err(ApiError::bad_request(
            "hash_mismatch",
            "содержимое не совпадает с хэшем",
        ));
    }
    let store = a.vault.blobs.clone();
    tokio::task::spawn_blocking(move || store.finalize(&path, &h)).await??;
    remove_row(&a, id).await?;
    Ok(Proto(pb::UploadState {
        upload_id: String::new(),
        offset: size,
        size,
        complete: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(range: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_str(range).unwrap());
        h
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(&HeaderMap::new(), 10), Ok(None));
        assert_eq!(parse_range(&hm("bytes=0-4"), 10), Ok(Some((0, 4))));
        assert_eq!(parse_range(&hm("bytes=5-"), 10), Ok(Some((5, 9))));
        assert_eq!(parse_range(&hm("bytes=-3"), 10), Ok(Some((7, 9))));
        assert_eq!(parse_range(&hm("bytes=3-100"), 10), Ok(Some((3, 9))));
        assert!(parse_range(&hm("bytes=10-"), 10).is_err());
        assert!(parse_range(&hm("bytes=0-1,3-4"), 10).is_err());
        assert!(parse_range(&hm("items=0-1"), 10).is_err());
    }

    #[test]
    fn content_range() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 0-99/1000"),
        );
        assert_eq!(parse_content_range(&h), Some((0, 99, 1000)));
    }
}
