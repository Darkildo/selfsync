//! Разрешение расхождений (раздел 8.4). Главный инвариант: ни одна версия текста не
//! исчезает молча. Любая неоднозначность — в пользу сохранения обеих версий.
//!
//! - правки в разных местах (diff3 без конфликтов) сливаются молча;
//! - пересекающиеся правки, бинарные файлы, файлы > 1 МиБ и «два устройства создали
//!   один путь» — обе версии целиком: серверная на месте, локальная рядом копией;
//! - правка всегда побеждает удаление.

use notesync_proto::v1 as pb;

use super::ctx::{Ctx, SMALL_BLOB, SyncError, SyncResult};
use super::transfer::{download_to, fetch_small};
use super::types::{Expect, LogLevel, Notice};
use crate::hash::Hash;
use crate::index::{ConflictRecord, FileState, LocalObs};
use crate::merge::{Merge, merge_bytes, mergeable};
use crate::path::VaultPath;

/// Состояние пути на сервере.
#[derive(Debug, Clone)]
pub(crate) struct Remote {
    pub rev: u64,
    pub hash: Option<Hash>,
    pub size: u64,
    pub deleted: bool,
    pub folder: bool,
    pub renamed_to: Option<VaultPath>,
}

impl Remote {
    pub fn from_entry(cx: &Ctx, e: &pb::Entry) -> Remote {
        let renamed_to = e
            .renamed_to
            .as_ref()
            .and_then(|p| cx.with(|s| s.decode_path(p)));
        Remote {
            rev: e.rev,
            hash: Hash::from_slice(&e.hash),
            size: e.size,
            deleted: e.deleted,
            folder: e.folder,
            renamed_to,
        }
    }
}

fn expect_of(obs: &Option<LocalObs>) -> Expect {
    match obs {
        Some(l) => Expect::Stat {
            size: l.size,
            mtime: l.mtime,
        },
        None => Expect::Absent,
    }
}

/// Дата для имён копий: `2026-10-09 14-30` (двоеточие недопустимо в именах на Windows).
pub(crate) fn stamp(now_ms: i64, tz_offset_min: i32) -> (String, String) {
    let t = now_ms / 1000 + i64::from(tz_offset_min) * 60;
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Гражданская дата из числа дней (алгоритм Хиннанта).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let date = format!("{y:04}-{m:02}-{d:02}");
    let time = format!("{:02}-{:02}", secs / 3600, (secs % 3600) / 60);
    (date, time)
}

/// Свободное имя копии рядом с файлом.
pub(crate) async fn unique_copy(cx: &Ctx, key: &str, label: &str) -> SyncResult<String> {
    let vp = VaultPath::parse(key).map_err(|e| SyncError::Io(e.to_string()))?;
    for n in 1..100 {
        let l = if n == 1 { label.to_owned() } else { format!("{label} {n}") };
        let cand = vp.with_suffix(&l).as_str().to_owned();
        let taken = cx.with(|s| s.index.files.contains_key(&cand) || s.is_excluded(&cand));
        if !taken && cx.stat(&cand).await?.is_none() {
            return Ok(cand);
        }
    }
    Err(SyncError::Io("не найдено свободное имя для копии".into()))
}

fn conflict_label(cx: &Ctx, initial: bool) -> String {
    cx.with(|s| {
        let (date, time) = stamp(s.now, s.cfg.tz_offset_min);
        if initial {
            format!("{} {date}", s.cfg.device_name)
        } else {
            format!("conflict {date} {time} {}", s.cfg.device_name)
        }
    })
}

/// Базовая версия текста: из кэша или с сервера (блоб жив, пока на него ссылается
/// история).
async fn base_content(cx: &Ctx, f: &FileState) -> Option<Vec<u8>> {
    let plain = f.base_plain?;
    if let Some(d) = cx.cache_read(&plain).await {
        return Some(d);
    }
    let blob = f.base_blob?;
    match fetch_small(cx, &blob).await {
        Ok(d) if Hash::of(&d) == plain => Some(d),
        _ => None,
    }
}

/// Принять серверную версию как базу (локальный файл уже совпадает с ней).
fn adopt(cx: &Ctx, key: &str, r: &Remote, plain: Hash) {
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.base_rev = r.rev;
            f.base_blob = r.hash;
            f.base_plain = Some(plain);
            f.server_path = None;
            f.rejected = None;
            f.pending_put = None;
        }
    });
}

fn add_conflict(cx: &Ctx, path: &str, copy: &str, initial: bool) {
    let id = cx.with_mut(|s| {
        s.index.next_conflict_id += 1;
        let id = s.index.next_conflict_id;
        if !initial {
            s.index.conflicts.push(ConflictRecord {
                id,
                path: path.to_owned(),
                copy: copy.to_owned(),
                at: s.now,
            });
        }
        id
    });
    cx.log(
        LogLevel::Warn,
        format!("конфликт: {path} — локальная версия сохранена как {copy}"),
    );
    if initial {
        cx.notify(Notice::LocalCopySaved {
            path: path.to_owned(),
            copy: copy.to_owned(),
        });
    } else {
        cx.notify(Notice::Conflict {
            id,
            path: path.to_owned(),
            copy: copy.to_owned(),
        });
    }
}

/// Сервер отклонил наш Put (или pull нашёл расхождение): разобраться и оставить
/// индекс в состоянии, из которого следующий push сделает правильное.
pub(crate) async fn resolve_content(cx: &Ctx, key: &str, r: Remote) -> SyncResult<()> {
    let Some(f) = cx.with(|s| s.index.files.get(key).cloned()) else {
        return Ok(());
    };
    // Свежее состояние локального файла.
    let Some(meta) = cx.stat(key).await? else {
        // Локальный файл так и не попал на сервер и уже удалён: серверная версия —
        // чужой файл, который здесь ещё не видели, а не повод для удаления.
        if !f.maybe_on_server() && !r.deleted && !r.folder
            && let Some(h) = r.hash
                && let Some((obs, cache)) = download_to(cx, key, &h, r.size, Expect::Absent).await? {
                    cx.with_mut(|s| {
                        if let Some(f) = s.index.files.get_mut(key) {
                            f.folder = false;
                            f.local = Some(obs);
                        }
                    });
                    adopt(cx, key, &r, obs.plain);
                    if let Some(c) = cache {
                        cx.cache_put(obs.plain, c).await;
                    }
                    return Ok(());
                }
        cx.with_mut(|s| {
            if let Some(f) = s.index.files.get_mut(key) {
                f.local = None;
            }
            s.dirty.insert(key.to_owned());
        });
        return Ok(());
    };
    if meta.dir {
        return Ok(());
    }

    if r.deleted {
        // Переименован на другом устройстве: правка следует за файлом.
        if let Some(to) = &r.renamed_to {
            let to = to.as_str().to_owned();
            let free = to != key && cx.with(|s| !s.index.files.contains_key(&to)) && cx.stat(&to).await?.is_none();
            if free && rename_local(cx, key, &to).await? {
                cx.with_mut(|s| {
                    if let Some(mut f) = s.index.files.remove(key) {
                        f.base_rev = 0;
                        f.base_blob = None;
                        f.server_path = None;
                        s.index.files.insert(to.clone(), f);
                    }
                });
                cx.log(LogLevel::Info, format!("{key} переименован на другом устройстве в {to}; правки перенесены"));
                cx.notify(Notice::FollowedRename {
                    from: key.to_owned(),
                    to,
                });
                return Ok(());
            }
        }
        // Правка побеждает удаление: файл возвращается.
        cx.with_mut(|s| {
            if let Some(f) = s.index.files.get_mut(key) {
                f.base_rev = r.rev;
                f.base_blob = None;
                f.server_path = None;
            }
        });
        cx.log(LogLevel::Info, format!("{key} удалён на другом устройстве, но изменён здесь — возвращается"));
        cx.notify(Notice::RestoredEdited { path: key.to_owned() });
        return Ok(());
    }

    if r.folder {
        // На сервере на этом месте папка: наш файл уходит в копию.
        let copy = unique_copy(cx, key, &conflict_label(cx, false)).await?;
        if rename_local(cx, key, &copy).await? {
            cx.with_mut(|s| {
                if let Some(mut f) = s.index.files.remove(key) {
                    f.base_rev = 0;
                    f.base_blob = None;
                    f.server_path = None;
                    s.index.files.insert(copy.clone(), f);
                }
            });
            add_conflict(cx, key, &copy, false);
        }
        return Ok(());
    }

    let Some(remote_hash) = r.hash else {
        return Err(SyncError::Protocol("живая запись без хэша".into()));
    };
    let small_local = meta.size <= SMALL_BLOB;
    let small_remote = r.size <= SMALL_BLOB;
    let local_bytes = if small_local { cx.read(key, 0, None).await? } else { None };
    let local_obs = match &local_bytes {
        Some(b) => LocalObs {
            size: b.len() as u64,
            mtime: meta.mtime,
            plain: Hash::of(b),
        },
        None => match f.local {
            Some(l) if l.size == meta.size && l.mtime == meta.mtime => l,
            _ => {
                // Большой файл изменился с последнего сканирования — пересканировать.
                cx.with_mut(|s| {
                    s.dirty.insert(key.to_owned());
                });
                return Ok(());
            }
        },
    };
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.local = Some(local_obs);
        }
    });

    let remote_bytes = if small_remote {
        Some(fetch_small(cx, &remote_hash).await?)
    } else {
        None
    };
    if let Some(sb) = &remote_bytes {
        let sp = Hash::of(sb);
        if sp == local_obs.plain {
            adopt(cx, key, &r, sp);
            if mergeable(sb) {
                cx.cache_put(sp, sb.clone()).await;
            }
            return Ok(());
        }
        if f.base_plain == Some(sp) {
            // На сервере та же база (сменилась только ревизия): наша правка в силе.
            adopt(cx, key, &r, sp);
            return Ok(());
        }
        if f.base_plain == Some(local_obs.plain) {
            // Мы ничего не меняли: просто серверная версия.
            return write_remote(cx, key, &r, sb.clone(), local_obs).await;
        }
        if let (Some(lb), true) = (&local_bytes, f.base_plain.is_some())
            && mergeable(lb) && mergeable(sb)
                && let Some(base) = base_content(cx, &f).await {
                    match merge_bytes(&base, lb, sb) {
                        Some(Merge::Clean(m)) => {
                            let m = m.into_bytes();
                            if Hash::of(&m) == sp {
                                return write_remote(cx, key, &r, sb.clone(), local_obs).await;
                            }
                            return write_merged(cx, key, &r, sb.clone(), m, local_obs).await;
                        }
                        Some(Merge::Conflict) | None => {}
                    }
                }
    }
    // Обе версии целиком.
    let initial = f.base_rev == 0 && f.base_plain.is_none();
    keep_both(cx, key, &r, remote_hash, local_bytes, local_obs, remote_bytes, initial).await
}

/// Записать серверную версию поверх неизменённой локальной.
async fn write_remote(cx: &Ctx, key: &str, r: &Remote, sb: Vec<u8>, local: LocalObs) -> SyncResult<()> {
    let sp = Hash::of(&sb);
    let size = sb.len() as u64;
    let cache = if mergeable(&sb) { Some(sb.clone()) } else { None };
    let Some(meta) = cx.write(key, sb, expect_of(&Some(local))).await? else {
        cx.with_mut(|s| {
            s.dirty.insert(key.to_owned());
        });
        return Ok(());
    };
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.local = Some(LocalObs {
                size,
                mtime: meta.mtime,
                plain: sp,
            });
        }
    });
    adopt(cx, key, r, sp);
    if let Some(c) = cache {
        cx.cache_put(sp, c).await;
    }
    Ok(())
}

/// Записать результат чистого слияния; следующий push отправит его.
async fn write_merged(cx: &Ctx, key: &str, r: &Remote, sb: Vec<u8>, merged: Vec<u8>, local: LocalObs) -> SyncResult<()> {
    let sp = Hash::of(&sb);
    let mp = Hash::of(&merged);
    let size = merged.len() as u64;
    let Some(meta) = cx.write(key, merged, expect_of(&Some(local))).await? else {
        cx.with_mut(|s| {
            s.dirty.insert(key.to_owned());
        });
        return Ok(());
    };
    cx.with_mut(|s| {
        if let Some(f) = s.index.files.get_mut(key) {
            f.local = Some(LocalObs {
                size,
                mtime: meta.mtime,
                plain: mp,
            });
            f.base_rev = r.rev;
            f.base_blob = r.hash;
            f.base_plain = Some(sp);
            f.server_path = None;
        }
    });
    cx.cache_put(sp, sb).await;
    cx.log(LogLevel::Info, format!("слито без конфликтов: {key}"));
    Ok(())
}

/// Пересекающиеся правки: серверная версия остаётся на месте, локальная — рядом.
#[allow(clippy::too_many_arguments)]
async fn keep_both(
    cx: &Ctx,
    key: &str,
    r: &Remote,
    remote_hash: Hash,
    local_bytes: Option<Vec<u8>>,
    local: LocalObs,
    remote_bytes: Option<Vec<u8>>,
    initial: bool,
) -> SyncResult<()> {
    let copy = unique_copy(cx, key, &conflict_label(cx, initial)).await?;
    let copy_obs;
    match local_bytes {
        Some(lb) => {
            // Копия — новым файлом, потом серверная версия на место.
            let Some(m) = cx.write(&copy, lb, Expect::Absent).await? else {
                cx.with_mut(|s| {
                    s.dirty.insert(copy.clone());
                });
                return Ok(());
            };
            copy_obs = LocalObs {
                size: local.size,
                mtime: m.mtime,
                plain: local.plain,
            };
            cx.with_mut(|s| {
                s.index.files.insert(
                    copy.clone(),
                    FileState {
                        local: Some(copy_obs),
                        ..Default::default()
                    },
                );
            });
            cx.save().await?;
            let written = match remote_bytes {
                Some(sb) => {
                    let sp = Hash::of(&sb);
                    let size = sb.len() as u64;
                    cx.write(key, sb, expect_of(&Some(local)))
                        .await?
                        .map(|m| LocalObs { size, mtime: m.mtime, plain: sp })
                }
                None => download_to(cx, key, &remote_hash, r.size, expect_of(&Some(local)))
                    .await?
                    .map(|x| x.0),
            };
            match written {
                Some(obs) => {
                    cx.with_mut(|s| {
                        if let Some(f) = s.index.files.get_mut(key) {
                            f.local = Some(obs);
                        }
                    });
                    adopt(cx, key, r, obs.plain);
                }
                None => {
                    // Пользователь правит файл прямо сейчас: копия уже сохранена,
                    // на следующем цикле разберёмся с новой версией.
                    cx.with_mut(|s| {
                        s.dirty.insert(key.to_owned());
                    });
                }
            }
        }
        None => {
            // Большой файл: переносим его в копию целиком и скачиваем серверный.
            let Some(m) = cx.rename(key, &copy).await? else {
                cx.with_mut(|s| {
                    s.dirty.insert(key.to_owned());
                });
                return Ok(());
            };
            copy_obs = LocalObs {
                size: local.size,
                mtime: m.mtime,
                plain: local.plain,
            };
            cx.with_mut(|s| {
                s.index.files.insert(
                    copy.clone(),
                    FileState {
                        local: Some(copy_obs),
                        ..Default::default()
                    },
                );
                if let Some(f) = s.index.files.get_mut(key) {
                    f.local = None;
                    f.base_rev = r.rev;
                    f.base_blob = r.hash;
                    f.server_path = None;
                }
            });
            cx.save().await?;
            if let Some((obs, _)) = download_to(cx, key, &remote_hash, r.size, Expect::Absent).await? {
                cx.with_mut(|s| {
                    if let Some(f) = s.index.files.get_mut(key) {
                        f.local = Some(obs);
                        f.base_plain = Some(obs.plain);
                    }
                });
            }
        }
    }
    add_conflict(cx, key, &copy, initial);
    cx.save().await
}

/// Наше локальное удаление отклонено: файл правили на другом устройстве.
pub(crate) async fn resolve_delete(cx: &Ctx, key: &str, r: Remote) -> SyncResult<()> {
    if r.deleted {
        cx.with_mut(|s| {
            s.index.files.remove(key);
        });
        return Ok(());
    }
    if r.folder {
        cx.mkdir(key).await?;
        cx.with_mut(|s| {
            if let Some(f) = s.index.files.get_mut(key) {
                f.base_rev = r.rev;
                f.local = Some(LocalObs {
                    size: 0,
                    mtime: 0,
                    plain: Hash::default(),
                });
            }
        });
        return Ok(());
    }
    let Some(h) = r.hash else {
        return Err(SyncError::Protocol("живая запись без хэша".into()));
    };
    // На сервере та же версия, что мы удалили (сменилась только ревизия, например
    // после сброса баз): повторить удаление с актуальной ревизией.
    let base = cx.with(|s| s.index.files.get(key).map(|f| (f.base_blob, f.base_plain.or(f.pending_put))));
    if let Some((base_blob, base_plain)) = base {
        let same = if base_blob == Some(h) {
            true
        } else if r.size <= SMALL_BLOB && base_plain.is_some() {
            fetch_small(cx, &h).await.ok().map(|d| Hash::of(&d)) == base_plain
        } else {
            false
        };
        if same {
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(key) {
                    f.base_rev = r.rev;
                    f.base_blob = Some(h);
                }
            });
            return Ok(());
        }
    }
    match download_to(cx, key, &h, r.size, Expect::Absent).await? {
        Some((obs, cache)) => {
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.get_mut(key) {
                    f.local = Some(obs);
                }
            });
            adopt(cx, key, &r, obs.plain);
            if let Some(c) = cache {
                cx.cache_put(obs.plain, c).await;
            }
            cx.log(LogLevel::Info, format!("{key}: удаление отменено — файл изменён на другом устройстве"));
            cx.notify(Notice::RestoredRemote { path: key.to_owned() });
        }
        None => {
            // Место занято файлом с другим регистром имени: вернуть серверную версию
            // сюда нельзя, она переименовывается на сервере.
            if let Some(existing) = case_twin(cx, key).await? {
                let server = cx.with(|s| s.encode_path(&VaultPath::parse(key).map_err(|e| SyncError::Io(e.to_string()))?))?;
                if split_case(cx, key, server, r.rev, &existing).await? {
                    cx.with_mut(|s| {
                        if s.index.files.get(key).is_some_and(|f| f.local.is_none()) {
                            s.index.files.remove(key);
                        }
                    });
                }
                return Ok(());
            }
            cx.with_mut(|s| {
                s.dirty.insert(key.to_owned());
            });
        }
    }
    Ok(())
}

/// Наше переименование отклонено.
pub(crate) async fn resolve_rename(cx: &Ctx, key: &str, r: Remote, at_destination: bool) -> SyncResult<()> {
    if at_destination {
        // На месте назначения другой файл: наш уходит в копию (на сервере его
        // переименование пойдёт уже в копию), серверный скачивается сразу — pull мог
        // пройти эту запись раньше.
        let copy = unique_copy(cx, key, &conflict_label(cx, false)).await?;
        if rename_local(cx, key, &copy).await? {
            cx.with_mut(|s| {
                if let Some(f) = s.index.files.remove(key) {
                    s.index.files.insert(copy.clone(), f);
                }
            });
            add_conflict(cx, key, &copy, false);
            cx.save().await?;
            if let (false, false, Some(h)) = (r.deleted, r.folder, r.hash) {
                if let Some((obs, cache)) = download_to(cx, key, &h, r.size, Expect::Absent).await? {
                    cx.with_mut(|s| {
                        s.index.files.insert(
                            key.to_owned(),
                            FileState {
                                local: Some(obs),
                                base_rev: r.rev,
                                base_blob: r.hash,
                                base_plain: Some(obs.plain),
                                ..Default::default()
                            },
                        );
                    });
                    if let Some(c) = cache {
                        cx.cache_put(obs.plain, c).await;
                    }
                } else {
                    cx.with_mut(|s| {
                        s.dirty.insert(key.to_owned());
                    });
                }
            }
        }
        return Ok(());
    }
    cx.with_mut(|s| {
        let Some(f) = s.index.files.get_mut(key) else { return };
        if r.deleted {
            // Источника на сервере больше нет: наш файл — новый файл по новому пути.
            f.server_path = None;
            f.base_rev = 0;
            f.base_blob = None;
            f.base_plain = None;
        } else {
            // Источник изменили: повторить переименование с его ревизией, а
            // содержимое сверить потом (base_blob = None → после rename base_rev = 0).
            f.base_rev = r.rev;
            if !f.folder {
                f.base_blob = None;
            }
        }
    });
    Ok(())
}

/// Регистронезависимая ФС: на месте `key` лежит файл с другим регистром имени.
pub(crate) async fn case_twin(cx: &Ctx, key: &str) -> SyncResult<Option<String>> {
    if !cx.with(|s| s.cfg.case_insensitive) {
        return Ok(None);
    }
    Ok(cx.stat(key).await?.filter(|m| !m.path.is_empty() && m.path != key).map(|m| m.path))
}

/// Два файла, различающиеся только регистром, здесь не уместить: серверный
/// переименовывается в свободное имя — сохраняются оба. Возвращает, применено ли
/// переименование.
pub(crate) async fn split_case(cx: &Ctx, key: &str, server: pb::Path, rev: u64, existing: &str) -> SyncResult<bool> {
    let label = {
        let (date, _) = stamp(cx.now(), cx.with(|s| s.cfg.tz_offset_min));
        format!("case {} {date}", cx.with(|s| s.cfg.device_name.clone()))
    };
    let copy = unique_copy(cx, key, &label).await?;
    let to = cx.with(|s| s.encode_path(&VaultPath::parse(&copy).map_err(|e| SyncError::Io(e.to_string()))?))?;
    let resp = super::api::ops(
        cx,
        vec![pb::Op {
            kind: Some(pb::op::Kind::Rename(pb::Rename {
                from: Some(server),
                to: Some(to),
                base_rev: rev,
            })),
        }],
    )
    .await?;
    let applied = matches!(resp.results.first().and_then(|r| r.result.as_ref()), Some(pb::op_result::Result::Applied(_)));
    if applied {
        cx.log(LogLevel::Warn, format!("{key} совпадает с {existing} без учёта регистра: переименован в {copy}"));
        cx.with_mut(|s| s.sync_due = Some(s.now));
    }
    if cx.with_mut(|s| s.notified.insert(format!("case:{key}"))) {
        cx.notify(Notice::CaseCollision {
            path: key.to_owned(),
            existing: existing.to_owned(),
        });
    }
    Ok(applied)
}

/// Локальное переименование (регистр меняется через временное имя).
pub(crate) async fn rename_local(cx: &Ctx, from: &str, to: &str) -> SyncResult<bool> {
    let case_only = from != to && from.to_lowercase() == to.to_lowercase();
    if case_only {
        let tmp = format!("{to}.case{}", super::ctx::TEMP_SUFFIX);
        if cx.rename(from, &tmp).await?.is_none() {
            return Ok(false);
        }
        if cx.rename(&tmp, to).await?.is_none() {
            cx.rename(&tmp, from).await?;
            return Ok(false);
        }
        return Ok(true);
    }
    Ok(cx.rename(from, to).await?.is_some())
}

#[cfg(test)]
mod tests {
    use super::stamp;

    #[test]
    fn stamps() {
        // 2026-10-09 14:30:00 UTC
        let t = 1_791_556_200_000;
        assert_eq!(stamp(t, 0), ("2026-10-09".into(), "14-30".into()));
        assert_eq!(stamp(t, 180), ("2026-10-09".into(), "17-30".into()));
        assert_eq!(stamp(t, -15 * 60), ("2026-10-08".into(), "23-30".into()));
        assert_eq!(stamp(0, 0), ("1970-01-01".into(), "00-00".into()));
    }
}
