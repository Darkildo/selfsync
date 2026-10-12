//! Блобы, которые просит сервер: `selfsync verify` нашёл их битыми или отсутствующими.
//! Блоб строится детерминированно (тот же открытый текст и ключ — те же байты), так что
//! устройство, у которого файл совпадает с базой, собирает его тем же кодом, что для Put,
//! и заливает; сервер проверит хэш.

use std::collections::BTreeSet;

use super::api;
use super::ctx::{Ctx, SMALL_BLOB, SyncResult};
use super::transfer::{Src, prepare_big, prepare_small, upload_big, upload_small};
use super::types::LogLevel;
use crate::hash::Hash;

/// Повторно разбирать тот же запрос не чаще (если число запрошенных и время `verify`
/// на сервере не изменились).
const RECHECK_MS: i64 = 60 * 60 * 1000;

pub(crate) async fn offer_wanted(cx: &Ctx) -> SyncResult<()> {
    let (count, verified_at, checked, now, busy) = cx.with(|s| {
        let vs = s.server_state.unwrap_or_default();
        (
            vs.blobs_wanted,
            vs.verified_at,
            s.wanted_checked,
            s.now,
            s.index.migration.is_some(),
        )
    });
    if count == 0 {
        // Запрос исполнен: новый с тем же числом блобов — уже другой.
        cx.with_mut(|s| s.wanted_checked = None);
        return Ok(());
    }
    if busy {
        return Ok(());
    }
    if checked.is_some_and(|(n, v, at)| n == count && v == verified_at && now - at < RECHECK_MS) {
        return Ok(());
    }
    let wanted: BTreeSet<Hash> = api::blobs_wanted(cx).await?.into_iter().collect();
    let keys = cx.with(|s| s.content_keys().map(|k| k.cloned()))?;
    // Файлы, которые здесь совпадают с запрошенной базой.
    let candidates: Vec<(String, Hash, crate::index::LocalObs)> = cx.with(|s| {
        s.index
            .files
            .iter()
            .filter(|(_, f)| !f.folder && f.clean() && f.transfer.is_none())
            .filter_map(|(k, f)| {
                let h = f.base_blob.filter(|h| wanted.contains(h))?;
                Some((k.clone(), h, f.local?))
            })
            .collect()
    });
    let mut sent: BTreeSet<Hash> = BTreeSet::new();
    for (key, h, obs) in candidates {
        if sent.contains(&h) {
            continue;
        }
        // Файл не трогали с последнего наблюдения.
        if !cx
            .stat(&key)
            .await?
            .is_some_and(|m| !m.dir && m.size == obs.size && m.mtime == obs.mtime)
        {
            continue;
        }
        let ok = if obs.size <= SMALL_BLOB {
            let Some(data) = cx.read(&key, 0, None).await? else {
                continue;
            };
            if Hash::of(&data) != obs.plain {
                continue;
            }
            let b = prepare_small(&data, obs.plain, keys.as_ref());
            if b.hash != h {
                continue;
            }
            upload_small(cx, &b).await?;
            true
        } else {
            match prepare_big(cx, Src::Vault(&key), &obs, keys.as_ref()).await? {
                Some(b) if b.hash == h => {
                    let done =
                        upload_big(cx, &key, Src::Vault(&key), &obs, &b, keys.as_ref()).await?;
                    if !done {
                        // Чекпоинт загрузки не нужен: обычный push этот файл не шлёт.
                        cx.with_mut(|s| {
                            if let Some(f) = s.index.files.get_mut(&key) {
                                f.transfer = None;
                            }
                        });
                    }
                    done
                }
                _ => false,
            }
        };
        if ok {
            cx.log(
                LogLevel::Info,
                format!("сервер просил блоб {key}: залит заново"),
            );
            sent.insert(h);
        }
    }
    cx.with_mut(|s| s.wanted_checked = Some((count, verified_at, now)));
    Ok(())
}
