//! Отправка локальных изменений: блобы (`missing` → загрузка) → `ops`.
//!
//! Порядок операций в раунде: папки, переименования, содержимое, удаления (файлы,
//! затем папки от глубоких к мелким). Переименование и правку одного файла
//! отправляют в разных раундах: Put по новому пути нужна ревизия после Rename.

use notesync_proto::v1 as pb;

use super::api;
use super::ctx::{Ctx, SMALL_BLOB, SyncError, SyncResult};
use super::resolve::{Remote, resolve_content, resolve_delete, resolve_rename};
use super::transfer::{PreparedBlob, Src, prepare_big, prepare_small, upload_big, upload_small};
use super::types::{LogLevel, Notice};
use crate::hash::Hash;
use crate::index::{FileState, LocalObs};
use crate::merge::mergeable;
use crate::path::VaultPath;

/// Максимум операций в одном запросе.
const OPS_BATCH: usize = 200;
/// Лимит байтов маленьких блобов в памяти за один батч.
const BATCH_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
enum Item {
    Mkdir { key: String },
    Rename { key: String, from: String },
    Put { key: String },
    Delete { key: String },
}

fn depth(p: &str) -> usize {
    p.matches('/').count()
}

fn plan(files: &std::collections::BTreeMap<String, FileState>, rebaselined: bool) -> Vec<Item> {
    let mut mkdirs = Vec::new();
    let mut renames = Vec::new();
    let mut puts = Vec::new();
    let mut del_files = Vec::new();
    let mut del_dirs = Vec::new();
    for (k, f) in files {
        if f.transfer.is_some() && f.local.is_none() {
            continue;
        }
        if let Some(from) = &f.server_path {
            if f.local.is_some() {
                renames.push((f.folder, depth(k), Item::Rename { key: k.clone(), from: from.clone() }));
            } else {
                // Переименовали и удалили до отправки: удаляем по старому пути.
                del_files.push(Item::Delete { key: k.clone() });
            }
            continue;
        }
        if f.folder {
            if f.local.is_some() && f.base_rev == 0 && f.rejected.is_none() && !rebaselined {
                mkdirs.push((depth(k), Item::Mkdir { key: k.clone() }));
            } else if f.delete_pending() {
                del_dirs.push((depth(k), Item::Delete { key: k.clone() }));
            }
            continue;
        }
        if f.content_dirty() {
            let (note, size) = (VaultPath::parse(k).is_ok_and(|p| p.is_note()), f.local.map_or(0, |l| l.size));
            puts.push((!note, size, Item::Put { key: k.clone() }));
        } else if f.delete_pending() {
            del_files.push(Item::Delete { key: k.clone() });
        }
    }
    mkdirs.sort_by_key(|x| x.0);
    // Папки раньше файлов, родители раньше детей.
    renames.sort_by_key(|x| (!x.0, x.1));
    puts.sort_by_key(|x| (x.0, x.1));
    del_dirs.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut out: Vec<Item> = mkdirs.into_iter().map(|x| x.1).collect();
    out.extend(renames.into_iter().map(|x| x.2));
    out.extend(puts.into_iter().map(|x| x.2));
    out.extend(del_files);
    out.extend(del_dirs.into_iter().map(|x| x.1));
    out
}

/// Подготовленная операция и то, что нужно для обработки её результата.
struct Prepared {
    item: Item,
    op: pb::Op,
    /// Put: открытый текст отправленной версии и блоб.
    sent: Option<(LocalObs, Hash)>,
    cache: Option<Vec<u8>>,
}

fn vp(key: &str) -> SyncResult<VaultPath> {
    VaultPath::parse(key).map_err(|e| SyncError::Io(format!("{key}: {e}")))
}

/// Отправляет всё, что изменилось локально. Возвращает, было ли что отправлять.
pub(crate) async fn push(cx: &Ctx) -> SyncResult<bool> {
    let mut did = false;
    for _round in 0..6 {
        let items = cx.with(|s| plan(&s.index.files, s.index.rebaselined));
        cx.log(LogLevel::Debug, format!("push plan {items:?}"));
        if items.is_empty() {
            break;
        }
        did = true;
        let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
        cx.set_status(|st| {
            st.total = total;
            st.done = 0;
        });
        let mut idx = 0;
        while idx < items.len() {
            // Батч: не больше OPS_BATCH операций и BATCH_BYTES маленьких блобов.
            let mut batch: Vec<Item> = Vec::new();
            let mut bytes = 0u64;
            while idx < items.len() && batch.len() < OPS_BATCH {
                let it = items[idx].clone();
                if let Item::Put { key } = &it {
                    let size = cx.with(|s| s.index.files.get(key).and_then(|f| f.local).map_or(0, |l| l.size));
                    if size <= SMALL_BLOB {
                        if bytes + size > BATCH_BYTES && !batch.is_empty() {
                            break;
                        }
                        bytes += size;
                    }
                }
                batch.push(it);
                idx += 1;
            }
            send_batch(cx, batch).await?;
            cx.set_status(|st| st.done = u32::try_from(idx).unwrap_or(u32::MAX));
        }
    }
    Ok(did)
}

async fn prepare_put(cx: &Ctx, key: &str, keys: Option<&crate::crypto::VaultKeys>) -> SyncResult<Option<(PreparedBlob, LocalObs, Option<Vec<u8>>, i64)>> {
    let Some(meta) = cx.stat(key).await? else {
        cx.with_mut(|s| {
            s.dirty.insert(key.to_owned());
        });
        return Ok(None);
    };
    if meta.dir {
        cx.with_mut(|s| {
            s.need_full_scan = true;
        });
        return Ok(None);
    }
    if meta.size <= SMALL_BLOB {
        let Some(data) = cx.read(key, 0, None).await? else {
            return Ok(None);
        };
        let obs = LocalObs {
            size: data.len() as u64,
            mtime: meta.mtime,
            plain: Hash::of(&data),
        };
        cx.with_mut(|s| {
            if let Some(f) = s.index.files.get_mut(key) {
                f.local = Some(obs);
            }
        });
        let b = prepare_small(&data, obs.plain, keys);
        let cache = if mergeable(&data) { Some(data) } else { None };
        return Ok(Some((b, obs, cache, meta.mtime)));
    }
    let obs = cx.with(|s| s.index.files.get(key).and_then(|f| f.local));
    let Some(obs) = obs.filter(|l| l.size == meta.size && l.mtime == meta.mtime) else {
        cx.with_mut(|s| {
            s.dirty.insert(key.to_owned());
        });
        return Ok(None);
    };
    match prepare_big(cx, Src::Vault(key), &obs, keys).await? {
        Some(b) => Ok(Some((b, obs, None, meta.mtime))),
        None => {
            cx.with_mut(|s| {
                s.dirty.insert(key.to_owned());
            });
            Ok(None)
        }
    }
}

async fn send_batch(cx: &Ctx, batch: Vec<Item>) -> SyncResult<()> {
    let keys = cx.with(|s| s.content_keys().map(|k| k.cloned()))?;
    let mut prepared: Vec<Prepared> = Vec::new();
    let mut blobs: Vec<(String, PreparedBlob, LocalObs)> = Vec::new();
    for item in batch {
        let (base_rev, folder) = match &item {
            Item::Mkdir { key } | Item::Rename { key, .. } | Item::Put { key } | Item::Delete { key } => {
                cx.with(|s| s.index.files.get(key).map(|f| (f.base_rev, f.folder)).unwrap_or((0, false)))
            }
        };
        let _ = folder;
        let op = match &item {
            Item::Mkdir { key } => {
                let path = cx.with(|s| s.to_server(&vp(key)?))?;
                pb::op::Kind::Mkdir(pb::Mkdir { path: Some(path) })
            }
            Item::Rename { key, from } => {
                let (to_p, from_p) = cx.with(|s| Ok::<_, SyncError>((s.to_server(&vp(key)?)?, s.to_server(&vp(from)?)?)))?;
                pb::op::Kind::Rename(pb::Rename {
                    from: Some(from_p),
                    to: Some(to_p),
                    base_rev,
                })
            }
            Item::Delete { key } => {
                let target = cx.with(|s| s.index.files.get(key).and_then(|f| f.server_path.clone())).unwrap_or_else(|| key.clone());
                let path = cx.with(|s| s.to_server(&vp(&target)?))?;
                pb::op::Kind::Delete(pb::Delete { path: Some(path), base_rev })
            }
            Item::Put { key } => {
                let Some((b, obs, cache, mtime)) = prepare_put(cx, key, keys.as_ref()).await? else {
                    continue;
                };
                let path = cx.with(|s| s.to_server(&vp(key)?))?;
                let op = pb::op::Kind::Put(pb::Put {
                    path: Some(path),
                    base_rev,
                    hash: b.hash.to_vec(),
                    size: b.len,
                    mtime,
                });
                let h = b.hash;
                blobs.push((key.clone(), b, obs));
                prepared.push(Prepared {
                    item,
                    op: pb::Op { kind: Some(op) },
                    sent: Some((obs, h)),
                    cache,
                });
                continue;
            }
        };
        prepared.push(Prepared {
            item,
            op: pb::Op { kind: Some(op) },
            sent: None,
            cache: None,
        });
    }
    if prepared.is_empty() {
        return Ok(());
    }

    // Блобы: что отсутствует на сервере — загрузить.
    if !blobs.is_empty() {
        let hashes: Vec<Hash> = blobs.iter().map(|b| b.1.hash).collect();
        let missing = api::blobs_missing(cx, &hashes).await?;
        let mut failed: Vec<String> = Vec::new();
        for (key, b, obs) in &blobs {
            if !missing.contains(&b.hash) {
                continue;
            }
            if b.data.is_some() {
                upload_small(cx, b).await?;
            } else if !upload_big(cx, key, Src::Vault(key), obs, b, keys.as_ref()).await? {
                failed.push(key.clone());
            }
        }
        if !failed.is_empty() {
            cx.with_mut(|s| {
                for k in &failed {
                    s.dirty.insert(k.clone());
                }
            });
            prepared.retain(|p| !matches!(&p.item, Item::Put { key } if failed.contains(key)));
        }
    }
    if prepared.is_empty() {
        return Ok(());
    }

    // До отправки: результат может не дойти (обрыв, убийство процесса) — тогда файл
    // «возможно, на сервере», и удаление/переименование пойдут через сервер.
    cx.with_mut(|s| {
        for p in &prepared {
            match &p.item {
                Item::Put { key } => {
                    if let (Some(f), Some((obs, _))) = (s.index.files.get_mut(key), &p.sent) {
                        if f.base_rev == 0 {
                            f.pending_put = Some(obs.plain);
                        }
                    }
                }
                Item::Mkdir { key } => {
                    if let Some(f) = s.index.files.get_mut(key) {
                        f.pending_put = Some(Hash::default());
                    }
                }
                _ => {}
            }
        }
    });
    cx.save().await?;
    let ops: Vec<pb::Op> = prepared.iter().map(|p| p.op.clone()).collect();
    let resp = api::ops(cx, ops).await?;
    if let Some(vs) = &resp.vault {
        cx.with_mut(|s| s.server_state = Some(vs.clone()));
    }
    if resp.results.len() != prepared.len() {
        return Err(SyncError::Protocol("число результатов не совпадает с числом операций".into()));
    }
    for (p, r) in prepared.into_iter().zip(resp.results) {
        handle_result(cx, p, r).await?;
    }
    cx.save().await
}

async fn handle_result(cx: &Ctx, p: Prepared, r: pb::OpResult) -> SyncResult<()> {
    use pb::op_result::Result as R;
    let Some(r) = r.result else {
        return Err(SyncError::Protocol("пустой результат операции".into()));
    };
    cx.log(
        LogLevel::Debug,
        format!(
            "push {:?} base {:?} -> {}",
            p.item,
            p.op.kind.as_ref().map(|k| match k {
                pb::op::Kind::Put(x) => x.base_rev,
                pb::op::Kind::Delete(x) => x.base_rev,
                pb::op::Kind::Rename(x) => x.base_rev,
                pb::op::Kind::Mkdir(_) => 0,
            }),
            match &r {
                R::Applied(a) => format!("applied rev {} noop {}", a.rev, a.noop),
                R::Conflict(c) => format!("conflict rev {:?} deleted {:?} dest {}", c.server.as_ref().map(|e| e.rev), c.server.as_ref().map(|e| e.deleted), c.at_destination),
                R::MissingBlob(_) => "missing blob".into(),
                R::Rejected(x) => format!("rejected {}", x.code),
            }
        ),
    );
    match (p.item, r) {
        (Item::Mkdir { key }, R::Applied(a)) => {
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(&key) {
                    f.base_rev = a.rev;
                    f.pending_put = None;
                }
            });
        }
        (Item::Rename { key, .. }, R::Applied(a)) => {
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(&key) {
                    f.server_path = None;
                    // Содержимое источника неизвестно (его меняли): сверить через Put.
                    f.base_rev = if !f.folder && f.base_blob.is_none() { 0 } else { a.rev };
                }
            });
        }
        (Item::Put { key }, R::Applied(a)) => {
            let Some((obs, h)) = p.sent else { return Ok(()) };
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(&key) {
                    f.base_rev = a.rev;
                    f.base_blob = Some(h);
                    f.base_plain = Some(obs.plain);
                    f.rejected = None;
                    f.transfer = None;
                    f.pending_put = None;
                }
            });
            if let Some(c) = p.cache {
                cx.cache_put(obs.plain, c).await;
            }
        }
        (Item::Delete { key }, R::Applied(_)) => {
            cx.with_mut(|s| {
                if s.index.files.get(&key).is_some_and(|f| f.local.is_none()) {
                    s.index.files.remove(&key);
                }
            });
        }
        (item, R::MissingBlob(_)) => {
            // Блоб пропал между загрузкой и операцией (gc): повторим в следующем раунде.
            cx.log(LogLevel::Warn, format!("блоб пропал на сервере, повтор: {item:?}"));
        }
        (item, R::Rejected(rj)) => {
            if rj.code == "plaintext_in_encrypted_vault" {
                return Err(SyncError::Restart);
            }
            let key = match &item {
                Item::Mkdir { key } | Item::Rename { key, .. } | Item::Put { key } | Item::Delete { key } => key.clone(),
            };
            let plain = p.sent.map(|s| s.0.plain).unwrap_or_default();
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(&key) {
                    f.rejected = Some((rj.code.clone(), plain));
                    if matches!(item, Item::Rename { .. }) {
                        // Переименование невозможно — отправить как новый файл.
                        f.server_path = None;
                        f.base_rev = 0;
                        f.base_blob = None;
                        f.base_plain = None;
                    }
                }
            });
            cx.log(LogLevel::Warn, format!("сервер отклонил {key}: {} {}", rj.code, rj.message));
            cx.notify(Notice::Rejected { path: key, code: rj.code });
        }
        (item, R::Conflict(c)) => {
            let Some(e) = c.server else {
                return Err(SyncError::Protocol("Conflict без записи".into()));
            };
            let remote = Remote::from_entry(cx, &e);
            match item {
                Item::Put { key } => resolve_content(cx, &key, remote).await?,
                Item::Delete { key } => {
                    // Удаление шло по серверному пути (переименовали и удалили до
                    // отправки): восстанавливать нужно там же.
                    let target = cx.with_mut(|s| {
                        let sp = s.index.files.get(&key).and_then(|f| f.server_path.clone());
                        match sp {
                            Some(sp) if sp != key && !s.index.files.contains_key(&sp) => {
                                if let Some(mut f) = s.index.files.remove(&key) {
                                    f.server_path = None;
                                    s.index.files.insert(sp.clone(), f);
                                }
                                sp
                            }
                            _ => key.clone(),
                        }
                    });
                    resolve_delete(cx, &target, remote).await?
                }
                Item::Rename { key, .. } => resolve_rename(cx, &key, remote, c.at_destination).await?,
                Item::Mkdir { key } => {
                    cx.log(LogLevel::Warn, format!("на месте папки {key} на сервере файл"));
                    cx.with_mut(|s| {
                        if let Some(f) = s.index.files.get_mut(&key) {
                            f.rejected = Some(("folder_conflict".into(), Hash::default()));
                        }
                    });
                }
            }
        }
    }
    Ok(())
}
