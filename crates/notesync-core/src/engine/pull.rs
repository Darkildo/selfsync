//! Получение изменений: `changes` → скачать нужное → атомарная запись → индекс.
//!
//! Записи собираются пачкой (до `BATCH` штук, все страницы), затем применяются в
//! порядке: папки, переименования, удаления, файлы (сначала заметки и мелкие, потом
//! вложения). Курсор `last_seq` сдвигается только после всей пачки; индекс
//! сохраняется после каждого файла, так что после перезапуска уже применённое
//! распознаётся как своё эхо и пропускается.

use std::collections::BTreeSet;

use notesync_proto::v1 as pb;

use super::api;
use super::crypto_flow::{check_state, rebaseline};
use super::ctx::{Ctx, SyncError, SyncResult};
use super::resolve::{Remote, case_twin, rename_local, resolve_content, resolve_delete, split_case};
use super::transfer::{download_to, read_local};
use super::types::{Expect, LogLevel, Notice};
use crate::hash::Hash;
use crate::index::{FileState, LocalObs};
use crate::path::VaultPath;

const PAGE: u32 = 1000;
const BATCH: usize = 20_000;

struct Item {
    key: String,
    e: pb::Entry,
    r: Remote,
}

/// Применяет удалённые изменения. Возвращает, изменилось ли что-то локально.
pub(crate) async fn pull(cx: &Ctx) -> SyncResult<bool> {
    let mut changed = false;
    loop {
        let since = cx.with(|s| s.index.last_seq);
        let mut cursor = since;
        let mut entries: Vec<pb::Entry> = Vec::new();
        let mut has_more;
        let mut first = true;
        loop {
            let resp = api::changes(cx, cursor, PAGE).await?;
            let vs = resp.vault.clone().unwrap_or_default();
            if first {
                first = false;
                check_state(cx, &vs).await?;
                if vs.seq < since {
                    // Сервер «моложе» клиента (восстановлен из бэкапа): полная сверка.
                    cx.log(LogLevel::Warn, format!("сервер вернулся с seq {since} на {}: полная сверка", vs.seq));
                    cx.notify(Notice::ServerRewound);
                    rebaseline(cx, true);
                    cx.save().await?;
                    return Err(SyncError::Restart);
                }
                if since > 0 && vs.purged_seq > since {
                    // Мы могли пропустить окончательно стёртые записи.
                    return full_reconcile(cx).await.map(|_| true);
                }
            }
            entries.extend(resp.entries);
            cursor = resp.next_seq.max(cursor);
            has_more = resp.has_more;
            if !has_more || entries.len() >= BATCH {
                break;
            }
        }
        cx.with_mut(|s| s.hold_seq = None);
        if !entries.is_empty() {
            changed |= apply_batch(cx, entries, false).await?;
        }
        let held = cx.with_mut(|s| {
            // Отложенная запись перечитается в следующем цикле.
            let held = s.hold_seq.take();
            s.index.last_seq = match held {
                Some(h) => cursor.min(h.saturating_sub(1)).max(since),
                None => cursor,
            };
            if !has_more {
                s.index.initial_done = true;
                s.index.rewound = false;
                if s.index.rebaselined && held.is_none() {
                    finish_rebaseline(s);
                }
            }
            held.is_some()
        });
        if held {
            cx.save().await?;
            break;
        }
        cx.save().await?;
        if !has_more {
            break;
        }
    }
    Ok(changed)
}

/// Полная сверка после сброса баз закончена: файлы, которых на сервере не нашлось,
/// отправляются как новые.
fn finish_rebaseline(s: &mut super::ctx::State) {
    s.index.rebaselined = false;
    let seen = std::mem::take(&mut s.index.rebaseline_seen);
    for (k, f) in s.index.files.iter_mut() {
        if f.base_rev == 0 && f.local.is_some() && f.server_path.is_none() && !seen.contains(k) {
            f.base_plain = None;
        }
    }
}

/// Полная сверка: перечитать весь лог и убрать то, чего на сервере больше нет.
async fn full_reconcile(cx: &Ctx) -> SyncResult<()> {
    let mut cursor = 0;
    let mut entries = Vec::new();
    loop {
        let resp = api::changes(cx, cursor, PAGE).await?;
        entries.extend(resp.entries);
        cursor = resp.next_seq.max(cursor);
        if !resp.has_more {
            break;
        }
    }
    apply_batch(cx, entries, true).await?;
    cx.with_mut(|s| {
        s.index.last_seq = cursor;
        s.index.initial_done = true;
        s.index.rewound = false;
    });
    cx.save().await
}

fn decode(cx: &Ctx, entries: Vec<pb::Entry>) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::with_capacity(entries.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Последняя запись пути побеждает (путь мог измениться между страницами).
    for e in entries.into_iter().rev() {
        let Some(p) = e.path.as_ref() else { continue };
        let Some(vp) = cx.with(|s| s.from_server(p)) else {
            if p.encrypted == cx.with(|s| s.encrypted()) {
                cx.log(LogLevel::Warn, format!("запись seq {} не расшифровывается — пропущена", e.seq));
            }
            continue;
        };
        let key = vp.as_str().to_owned();
        if cx.with(|s| s.is_excluded(&key)) || !seen.insert(key.clone()) {
            continue;
        }
        let r = Remote::from_entry(cx, &e);
        out.push(Item { key, e, r });
    }
    out.reverse();
    out
}

async fn apply_batch(cx: &Ctx, entries: Vec<pb::Entry>, full_listing: bool) -> SyncResult<bool> {
    let items = decode(cx, entries);
    let listed: BTreeSet<String> = items.iter().map(|i| i.key.clone()).collect();
    cx.with_mut(|s| {
        if s.index.rebaselined {
            s.index.rebaseline_seen.extend(listed.iter().cloned());
        }
    });
    let mut dirs = Vec::new();
    let mut renames = Vec::new();
    let mut tombs = Vec::new();
    let mut files = Vec::new();
    for it in items {
        if it.e.deleted {
            tombs.push(it);
        } else if it.e.folder {
            dirs.push(it);
        } else if it.e.renamed_from.is_some() {
            renames.push(it);
        } else {
            files.push(it);
        }
    }
    dirs.sort_by_key(|i| i.key.matches('/').count());
    tombs.sort_by_key(|i| (i.e.folder, std::cmp::Reverse(i.key.matches('/').count())));
    files.sort_by_key(|i| (!VaultPath::parse(&i.key).is_ok_and(|p| p.is_note()), i.e.size));

    let total = u32::try_from(dirs.len() + renames.len() + tombs.len() + files.len()).unwrap_or(u32::MAX);
    let mut done = 0u32;
    cx.set_status(|st| {
        st.total = total;
        st.done = 0;
    });
    let mut changed = false;
    for it in dirs {
        changed |= apply_folder(cx, &it).await?;
        done += 1;
    }
    for it in renames.into_iter().chain(files) {
        changed |= guard(cx, &it, apply_file(cx, &it).await)?;
        done += 1;
        if done % 16 == 0 {
            cx.set_status(|st| st.done = done);
        }
        cx.save().await?;
    }
    for it in tombs {
        changed |= guard(cx, &it, apply_tombstone(cx, &it).await)?;
    }
    if full_listing {
        changed |= drop_missing(cx, &listed).await?;
    }
    cx.set_status(|st| st.done = total);
    Ok(changed)
}

/// Ошибка одного файла не должна останавливать остальные (кроме сетевых и
/// блокирующих — с ними цикл прерывается и повторится целиком).
fn guard(cx: &Ctx, it: &Item, r: SyncResult<bool>) -> SyncResult<bool> {
    let key = &it.key;
    match r {
        Ok(c) => Ok(c),
        Err(e @ (SyncError::BlobGone | SyncError::Corrupt(_))) => {
            hold(cx, it.e.seq);
            cx.log(LogLevel::Error, format!("{key}: {e}"));
            cx.notify(Notice::Error {
                code: "download_failed".into(),
                message: format!("{key}: {e}"),
            });
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

async fn apply_folder(cx: &Ctx, it: &Item) -> SyncResult<bool> {
    let existing = cx.with(|s| s.index.files.get(&it.key).map(|f| f.folder));
    match existing {
        Some(true) => {
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(&it.key) {
                    f.base_rev = it.r.rev;
                    if f.local.is_none() {
                        // Папку удалили локально, а на сервере её снова создали — вернуть.
                        f.local = Some(dir_obs());
                    }
                }
            });
            cx.mkdir(&it.key).await?;
            Ok(false)
        }
        Some(false) => {
            cx.log(LogLevel::Warn, format!("на сервере папка {} на месте локального файла", it.key));
            Ok(false)
        }
        None => {
            cx.mkdir(&it.key).await?;
            cx.with_mut(|s| {
                s.index.files.insert(
                    it.key.clone(),
                    FileState {
                        folder: true,
                        base_rev: it.r.rev,
                        local: Some(dir_obs()),
                        ..Default::default()
                    },
                );
            });
            Ok(true)
        }
    }
}

fn dir_obs() -> LocalObs {
    LocalObs {
        size: 0,
        mtime: 0,
        plain: Hash::default(),
    }
}

async fn apply_file(cx: &Ctx, it: &Item) -> SyncResult<bool> {
    let key = &it.key;
    let r = &it.r;
    let f = cx.with(|s| s.index.files.get(key).cloned());
    cx.log(
        LogLevel::Debug,
        format!(
            "pull file {key} rev {} seq {} from {:?}; index {:?}",
            r.rev,
            it.e.seq,
            it.e.renamed_from.as_ref().and_then(|p| cx.with(|s| s.from_server(p))),
            f.as_ref().map(|f| (f.base_rev, f.local.is_some(), f.clean(), f.server_path.clone()))
        ),
    );
    // Своё эхо или уже применено.
    if let Some(f) = &f {
        if r.hash.is_some() && f.base_rev == r.rev && f.base_blob == r.hash {
            return Ok(false);
        }
    }
    // Переименование файла, который у нас есть: локальный rename без скачивания.
    if let Some(from) = it.e.renamed_from.as_ref().and_then(|p| cx.with(|s| s.from_server(p))) {
        let from = from.as_str().to_owned();
        let src = cx.with(|s| s.index.files.get(&from).cloned());
        if let Some(src) = src {
            if from != *key && f.is_none() && src.local.is_some() && src.server_path.is_none() && r.hash.is_some() {
                // На регистронезависимой ФС stat нового имени при смене только регистра
                // находит сам источник — это не занятое место.
                let free = match cx.stat(key).await? {
                    None => true,
                    Some(m) => m.path == from,
                };
                if free && rename_local(cx, &from, key).await? {
                    let unchanged = src.base_blob == r.hash;
                    cx.with_mut(|s| {
                        if let Some(mut moved) = s.index.files.remove(&from) {
                            if unchanged {
                                moved.base_rev = r.rev;
                                moved.base_blob = r.hash;
                            } else {
                                // Содержимое на сервере сменилось вместе с именем: базу
                                // сверит resolve_content (base_plain остаётся предком).
                                moved.base_rev = 0;
                                moved.base_blob = None;
                            }
                            s.index.files.insert(key.clone(), moved);
                        }
                        s.dirty.insert(key.clone());
                    });
                    if !unchanged {
                        cx.save().await?;
                        resolve_content(cx, key, r.clone()).await?;
                        hold_unsettled(cx, key, it);
                    }
                    return Ok(true);
                }
            }
        }
    }
    // Регистронезависимая ФС: другой файл с тем же именем без учёта регистра.
    if cx.with(|s| s.cfg.case_insensitive) && f.is_none() {
        let fold = key.to_lowercase();
        let mut other = cx.with(|s| {
            s.index
                .files
                .iter()
                .find(|(k, f)| *k != key && k.to_lowercase() == fold && f.local.is_some())
                .map(|(k, _)| k.clone())
        });
        if other.is_none() {
            other = case_twin(cx, key).await?;
        }
        if let Some(existing) = other {
            hold(cx, it.e.seq);
            if let Some(path) = &it.e.path {
                split_case(cx, key, path.clone(), r.rev, &existing).await?;
            }
            return Ok(false);
        }
    }
    let Some(hash) = r.hash else {
        return Err(SyncError::Protocol("живая запись без хэша".into()));
    };
    match f {
        Some(f) if f.delete_pending() => {
            // Удалено здесь, изменено там: правка побеждает удаление.
            resolve_delete(cx, key, r.clone()).await?;
            hold_unsettled(cx, key, it);
            Ok(true)
        }
        Some(f) if f.clean() => match download_to(cx, key, &hash, r.size, Expect::Stat { size: f.local.map_or(0, |l| l.size), mtime: f.local.map_or(0, |l| l.mtime) }).await? {
            Some((obs, cache)) => {
                set_synced(cx, key, r, obs);
                if let Some(c) = cache {
                    cx.cache_put(obs.plain, c).await;
                }
                Ok(true)
            }
            None => {
                // Файл меняют прямо сейчас: перечитать запись в следующем цикле.
                hold(cx, it.e.seq);
                cx.with_mut(|s| {
                    s.dirty.insert(key.clone());
                });
                Ok(false)
            }
        },
        Some(f) if f.local.is_some() => {
            // Локальные правки (или неизвестная база): разрешение расхождения.
            resolve_content(cx, key, r.clone()).await?;
            hold_unsettled(cx, key, it);
            Ok(true)
        }
        Some(_) | None => {
            // Локально файла нет в индексе (или он ещё не скачан).
            if let Some(meta) = cx.stat(key).await? {
                if meta.dir {
                    return Ok(false);
                }
                if !meta.path.is_empty() && meta.path != *key {
                    // Регистронезависимая ФС: это другой файл.
                    hold(cx, it.e.seq);
                    if let Some(path) = &it.e.path {
                        split_case(cx, key, path.clone(), r.rev, &meta.path).await?;
                    }
                    return Ok(false);
                }
                // Файл есть, но не в индексе (первичная загрузка): сначала сравнить.
                let Some(content) = read_local(cx, key, &meta).await? else {
                    return Ok(false);
                };
                cx.with_mut(|s| {
                    let e = s.index.files.entry(key.clone()).or_default();
                    e.local = Some(content.obs);
                });
                let keys = cx.with(|s| s.content_keys().map(|k| k.cloned()))?;
                let same = match &content.data {
                    Some(d) => crate::blob::blob_hash(d, content.obs.plain, keys.as_ref()) == hash,
                    None => false,
                };
                if same {
                    set_synced(cx, key, r, content.obs);
                    return Ok(false);
                }
                resolve_content(cx, key, r.clone()).await?;
                hold_unsettled(cx, key, it);
                return Ok(true);
            }
            match download_to(cx, key, &hash, r.size, Expect::Absent).await? {
                Some((obs, cache)) => {
                    set_synced(cx, key, r, obs);
                    if let Some(c) = cache {
                        cx.cache_put(obs.plain, c).await;
                    }
                    Ok(true)
                }
                None => {
                    // Пока скачивали, здесь появился файл с тем же именем: перечитать
                    // запись в следующем цикле, когда он будет в индексе.
                    hold(cx, it.e.seq);
                    cx.with_mut(|s| {
                        s.dirty.insert(key.clone());
                    });
                    Ok(false)
                }
            }
        }
    }
}

/// Разрешение расхождения не довелось до конца (файл меняли посреди записи и т.п.),
/// и серверная ревизия не принята в индекс: перечитать запись в следующем цикле.
/// Запись, ушедшая с этого пути (переименование вслед за сервером, копия), считается
/// разобранной.
fn hold_unsettled(cx: &Ctx, key: &str, it: &Item) {
    if cx.with(|s| s.index.files.get(key).is_some_and(|f| f.base_rev != it.r.rev)) {
        hold(cx, it.e.seq);
    }
}

/// Отложить запись: курсор не сдвинется дальше неё.
fn hold(cx: &Ctx, seq: u64) {
    cx.with_mut(|s| s.hold_seq = Some(s.hold_seq.map_or(seq, |h| h.min(seq))));
}

fn set_synced(cx: &Ctx, key: &str, r: &Remote, obs: LocalObs) {
    cx.with_mut(|s| {
        let f = s.index.files.entry(key.to_owned()).or_default();
        f.folder = false;
        f.local = Some(obs);
        f.base_rev = r.rev;
        f.base_blob = r.hash;
        f.base_plain = Some(obs.plain);
        f.server_path = None;
        f.rejected = None;
        f.transfer = None;
        f.pending_put = None;
    });
}

async fn apply_tombstone(cx: &Ctx, it: &Item) -> SyncResult<bool> {
    let key = &it.key;
    let r = &it.r;
    let (initial_done, rewound, f) = cx.with(|s| (s.index.initial_done, s.index.rewound, s.index.files.get(key).cloned()));
    cx.log(
        LogLevel::Debug,
        format!(
            "pull tomb {key} rev {} initial_done {initial_done}; index {:?}",
            r.rev,
            f.as_ref().map(|f| (f.base_rev, f.local.is_some(), f.clean(), f.server_path.clone()))
        ),
    );
    let Some(f) = f else { return Ok(false) };
    // Первичная загрузка не удаляет файлы, лежавшие в папке до подключения. Файл,
    // который уже бывал синхронизирован (скачан прерванной первой загрузкой, отправлен
    // отсюда; после сброса баз от этого остаётся base_plain), защищать не от чего —
    // кроме случая откатившегося сервера.
    let synced_before = f.base_rev > 0 || f.base_plain.is_some();
    if !initial_done && (rewound || !synced_before) {
        return Ok(false);
    }
    if f.base_rev >= r.rev || f.server_path.is_some() {
        return Ok(false);
    }
    if f.folder {
        if f.local.is_some() {
            if cx.rmdir(key).await? {
                cx.with_mut(|s| {
                    s.index.files.remove(key);
                });
            } else {
                // В папке есть локальные файлы — папка остаётся и будет создана заново.
                cx.with_mut(|s| {
                    if let Some(f) = s.index.files.get_mut(key) {
                        f.base_rev = 0;
                    }
                });
            }
        } else {
            cx.with_mut(|s| {
                s.index.files.remove(key);
            });
        }
        return Ok(true);
    }
    if f.local.is_none() {
        cx.with_mut(|s| {
            s.index.files.remove(key);
        });
        return Ok(false);
    }
    if f.clean() {
        let l = f.local.unwrap_or(dir_obs());
        if cx.trash(key, Expect::Stat { size: l.size, mtime: l.mtime }).await? {
            cx.with_mut(|s| {
                s.index.files.remove(key);
            });
        } else {
            // Файл изменился с последнего наблюдения: применить tombstone заново в
            // следующем цикле (правка победит удаление, если содержимое другое).
            hold(cx, it.e.seq);
            cx.with_mut(|s| {
                s.dirty.insert(key.clone());
            });
        }
        return Ok(true);
    }
    // Правка побеждает удаление: Put с base_rev tombstone'а вернёт файл.
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.base_rev = r.rev;
            f.base_blob = None;
        }
    });
    cx.log(LogLevel::Info, format!("{key} удалён на другом устройстве, но изменён здесь — возвращается"));
    cx.notify(Notice::RestoredEdited { path: key.clone() });
    Ok(false)
}

/// После полной сверки: записей, известных индексу, на сервере больше нет совсем.
async fn drop_missing(cx: &Ctx, listed: &BTreeSet<String>) -> SyncResult<bool> {
    let missing: Vec<(String, FileState)> = cx.with(|s| {
        s.index
            .files
            .iter()
            .filter(|(k, f)| f.base_rev > 0 && f.server_path.is_none() && !listed.contains(*k))
            .map(|(k, f)| (k.clone(), f.clone()))
            .collect()
    });
    let mut changed = false;
    for (k, f) in missing {
        if f.local.is_none() {
            cx.with_mut(|s| {
                s.index.files.remove(&k);
            });
        } else if f.clean() && !f.folder {
            let l = f.local.unwrap_or(dir_obs());
            if cx.trash(&k, Expect::Stat { size: l.size, mtime: l.mtime }).await? {
                cx.with_mut(|s| {
                    s.index.files.remove(&k);
                });
                changed = true;
            }
        } else {
            // Изменён локально (или папка) — отправить заново как новый.
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(&k) {
                    f.base_rev = 0;
                    f.base_blob = None;
                    f.base_plain = None;
                }
            });
        }
    }
    Ok(changed)
}
