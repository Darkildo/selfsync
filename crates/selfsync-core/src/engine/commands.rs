//! Команды интерфейса (окно восстановления, история, устройства, подключение по QR,
//! настройки vault'а) и решения пользователя по конфликтам.

use selfsync_proto::v1 as pb;
use selfsync_proto::{MINOR_READ_ONLY, MINOR_REKEY};

use super::api;
use super::ctx::{Ctx, SMALL_BLOB, SyncError, SyncResult};
use super::resolve::unique_copy;
use super::transfer::{download_to, fetch_small};
use super::types::*;
use crate::hash::Hash;
use crate::index::FileState;
use crate::path::VaultPath;

fn vp(p: &str) -> SyncResult<VaultPath> {
    VaultPath::normalize(p).map_err(|e| SyncError::Http {
        status: 400,
        code: e.code().into(),
        message: e.to_string(),
    })
}

fn error_result(e: &SyncError) -> UiResult {
    let code = match e {
        SyncError::Network(_) => "offline",
        SyncError::Unauthorized => "unauthorized",
        SyncError::RateLimited { .. } => "rate_limited",
        SyncError::ServerOutdated(_) => "server_outdated",
        SyncError::ReadOnly => "read_only",
        SyncError::Paused(r) | SyncError::Blocked(r) => r.as_str(),
        SyncError::Http { code, .. } | SyncError::Server { code, .. } => code.as_str(),
        _ => "error",
    };
    UiResult::Error {
        code: code.to_owned(),
        message: e.to_string(),
        retry_after: match e {
            SyncError::RateLimited { retry_after_s } => *retry_after_s,
            _ => 0,
        },
    }
}

/// Проверить, что смена ключа возможна, и поставить её в ближайший цикл синка:
/// перешифровка долгая и должна переживать перезапуск, поэтому её ведёт цикл.
async fn start_rekey(cx: &Ctx, password: String, remember: bool) -> SyncResult<UiResult> {
    let fail = |code: &str, message: &str| {
        Err(SyncError::Http {
            status: 409,
            code: code.into(),
            message: message.into(),
        })
    };
    if !cx.with(|s| s.encrypted()) {
        return fail("no_vault_key", "vault не зашифрован");
    }
    if cx.with(|s| s.index.migration.is_some() || s.rekey.is_some()) {
        return fail("rekey_in_progress", "перешифровка уже идёт");
    }
    if server_minor(cx).await? < MINOR_REKEY {
        return Err(SyncError::ServerOutdated(
            "сервер не умеет смену ключа: обновите его до 0.3.0 или новее".into(),
        ));
    }
    // Пароль проверяется сразу, а не в цикле: окну нужен ответ «неверный пароль».
    let vk = api::get_vault_key(cx).await?;
    if crate::crypto::has_prev(&vk.record) {
        return fail(
            "rekey_unfinished",
            "предыдущая смена ключа ещё не завершена",
        );
    }
    match crate::crypto::open_master_key(&vk.record, &password) {
        Ok(_) => {}
        Err(crate::crypto::CryptoError::WrongPassword) => {
            return fail("wrong_password", "неверный пароль");
        }
        Err(e) => return fail("bad_vault_key", &e.to_string()),
    }
    cx.with_mut(|s| {
        s.rekey = Some((password, remember));
        s.sync_due = Some(s.now);
    });
    Ok(UiResult::Ok)
}

/// Удалить локальные копии под исключённым путём, которые при забывании совпадали
/// с сервером и с тех пор не менялись. Удаление не уходит на сервер: исключённых
/// путей в индексе нет, скан их не видит.
async fn remove_local(cx: &Ctx, path: &str) -> SyncResult<UiResult> {
    let p = vp(path)?;
    let root = p.as_str().to_owned();
    let under = |k: &str| k == root || k.starts_with(&format!("{root}/"));
    if !cx.with(|s| s.is_excluded(&root)) {
        return Err(SyncError::Http {
            status: 409,
            code: "not_excluded".into(),
            message: format!(
                "{root}: путь не исключён на этом устройстве (или правила ещё не применены)"
            ),
        });
    }
    let targets: Vec<(String, crate::index::LocalObs)> = cx.with(|s| {
        s.index
            .offloadable
            .iter()
            .filter(|(k, _)| under(k) && s.is_excluded(k) && !s.index.files.contains_key(*k))
            .map(|(k, l)| (k.clone(), *l))
            .collect()
    });
    let mut removed = 0u32;
    let mut dirs = std::collections::BTreeSet::new();
    for (k, l) in targets {
        let gone = cx
            .delete(
                &k,
                Expect::Stat {
                    size: l.size,
                    mtime: l.mtime,
                },
            )
            .await?;
        // Изменён после исключения — оставить; в любом случае больше не «можно удалить».
        cx.with_mut(|s| s.index.offloadable.remove(&k));
        if gone {
            removed += 1;
            let mut d = k.as_str();
            while let Some((parent, _)) = d.rsplit_once('/') {
                if !under(parent) {
                    break;
                }
                dirs.insert(parent.to_owned());
                d = parent;
            }
        }
    }
    // Опустевшие папки — от глубоких к мелким; непустые остаются.
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for d in dirs {
        cx.rmdir(&d).await?;
    }
    cx.save().await?;
    let kept = cx
        .list()
        .await?
        .iter()
        .filter(|m| !m.dir && under(&m.path))
        .count();
    cx.log(
        LogLevel::Info,
        format!("{root}: удалено локальных копий {removed}, оставлено {kept}"),
    );
    Ok(UiResult::Removed {
        removed,
        kept: u32::try_from(kept).unwrap_or(u32::MAX),
    })
}

/// Минорная версия протокола сервера; ещё ни одного ответа — спросить health.
async fn server_minor(cx: &Ctx) -> SyncResult<u32> {
    if let Some(m) = cx.with(|s| s.status.server_minor) {
        return Ok(m);
    }
    api::health(cx).await?;
    Ok(cx.with(|s| s.status.server_minor).unwrap_or(0))
}

/// Команда меняет что-то на сервере (устройству только для чтения нельзя).
fn writes(cmd: &UiCommand) -> bool {
    matches!(
        cmd,
        UiCommand::RestoreDeleted { .. }
            | UiCommand::PurgeDeleted { .. }
            | UiCommand::RestoreRevision { .. }
            | UiCommand::RevokeDevice { .. }
            | UiCommand::CreateJoin { .. }
            | UiCommand::SetRetention { .. }
            | UiCommand::CreateRecovery { .. }
            | UiCommand::RemoveRecovery { .. }
            | UiCommand::Recover { .. }
            | UiCommand::Rekey { .. }
    )
}

pub(crate) async fn run(cx: &Ctx, req: u64, cmd: UiCommand) {
    let res = if writes(&cmd) && cx.with(|s| s.read_only) {
        Err(SyncError::ReadOnly)
    } else {
        exec(cx, cmd).await
    };
    let result = match res {
        Ok(r) => r,
        Err(e) => {
            if matches!(e, SyncError::ReadOnly) {
                cx.with_mut(|s| s.set_read_only(None));
            }
            error_result(&e)
        }
    };
    cx.hub.emit(Action::UiResult { req, result });
}

/// Размер файла ревизии; сервер старше 1.3 его не присылает — тогда размер блоба.
fn file_size(r: &pb::Revision) -> u64 {
    r.plain_size.unwrap_or(r.size)
}

async fn exec(cx: &Ctx, cmd: UiCommand) -> SyncResult<UiResult> {
    match cmd {
        UiCommand::ListDeleted => {
            let d = api::deleted(cx).await?;
            let mut items = Vec::new();
            for it in d.items {
                let Some(e) = it.entry else { continue };
                let Some(path) = e.path.as_ref().and_then(|p| cx.with(|s| s.decode_path(p))) else {
                    continue;
                };
                items.push(DeletedView {
                    path: path.as_str().to_owned(),
                    deleted_at: e.deleted_at,
                    expires_at: it.expires_at,
                    size: it.last_live.as_ref().map_or(0, file_size),
                    device: e.updated_by,
                });
            }
            Ok(UiResult::Deleted { items })
        }
        UiCommand::RestoreDeleted { paths } => {
            let d = api::deleted(cx).await?;
            let mut ops = Vec::new();
            for it in d.items {
                let (Some(e), Some(last)) = (it.entry, it.last_live) else {
                    continue;
                };
                let Some(path) = e.path.as_ref().and_then(|p| cx.with(|s| s.decode_path(p))) else {
                    continue;
                };
                if !paths.iter().any(|p| p == path.as_str()) {
                    continue;
                }
                // Восстановление — обычный Put старого хэша с base_rev tombstone'а.
                ops.push(pb::Op {
                    kind: Some(pb::op::Kind::Put(pb::Put {
                        path: e.path,
                        base_rev: e.rev,
                        hash: last.hash,
                        size: last.size,
                        mtime: last.mtime,
                    })),
                });
            }
            let count = if ops.is_empty() {
                0
            } else {
                api::ops(cx, ops)
                    .await?
                    .results
                    .iter()
                    .filter(|r| matches!(r.result, Some(pb::op_result::Result::Applied(_))))
                    .count()
            };
            cx.with_mut(|s| s.sync_due = Some(s.now));
            Ok(UiResult::Restored {
                count: u32::try_from(count).unwrap_or(u32::MAX),
            })
        }
        UiCommand::PurgeDeleted { paths } => {
            let mut sp = Vec::new();
            for p in paths {
                let v = vp(&p)?;
                sp.push(cx.with(|s| s.encode_path(&v))?);
            }
            let r = api::purge_deleted(cx, sp).await?;
            Ok(UiResult::Restored {
                count: u32::try_from(r.purged).unwrap_or(u32::MAX),
            })
        }
        UiCommand::History { path } => {
            let v = vp(&path)?;
            let sp = cx.with(|s| s.encode_path(&v))?;
            let h = api::history(cx, &sp).await?;
            let revisions = h
                .revisions
                .into_iter()
                .map(|r| RevisionView {
                    rev: r.rev,
                    size: file_size(&r),
                    mtime: r.mtime,
                    seq: r.seq,
                    device: r.updated_by,
                    deleted: r.deleted,
                    renamed_from: r
                        .renamed_from
                        .as_ref()
                        .and_then(|p| cx.with(|s| s.decode_path(p)))
                        .map(|p| p.as_str().to_owned()),
                })
                .collect();
            Ok(UiResult::History { revisions })
        }
        UiCommand::RestoreRevision { path, rev } => restore_revision(cx, &path, rev).await,
        UiCommand::Devices => {
            let d = api::devices(cx).await?;
            cx.with_mut(|s| s.device_id = d.self_id);
            Ok(UiResult::Devices {
                vault: d.vault,
                devices: d
                    .devices
                    .into_iter()
                    .map(|x| DeviceView {
                        current: x.id == d.self_id,
                        id: x.id,
                        name: x.name,
                        created_at: x.created_at,
                        last_seen: x.last_seen,
                        revoked: x.revoked,
                        read_only: x.read_only,
                        client: x.client,
                    })
                    .collect(),
            })
        }
        UiCommand::RevokeDevice { id } => {
            api::revoke_device(cx, id).await?;
            Ok(UiResult::Ok)
        }
        UiCommand::CreateJoin { name, read_only } => {
            // Сервер 0.1.x поле `read_only` молча отбросил бы и выдал полный доступ.
            if read_only && server_minor(cx).await? < MINOR_READ_ONLY {
                return Err(SyncError::ServerOutdated(
                    "сервер не поддерживает устройства только для чтения: обновите его до 0.2.1 или новее".into(),
                ));
            }
            let j = api::join_create(cx, name, read_only).await?;
            Ok(UiResult::Join {
                code: j.code,
                url: j.url,
                expires_at: j.expires_at,
            })
        }
        UiCommand::Redeem { code, name } => {
            let t = api::join_redeem(cx, code, name).await?;
            Ok(UiResult::Token {
                token: t.token,
                vault: t.vault,
                device_id: t.device_id,
                device_name: t.device_name,
            })
        }
        UiCommand::GetRetention => Ok(UiResult::Retention {
            days: api::get_retention(cx).await?,
        }),
        UiCommand::SetRetention { days } => Ok(UiResult::Retention {
            days: api::set_retention(cx, days).await?,
        }),
        UiCommand::Stats => {
            let s = api::stats(cx).await?;
            Ok(UiResult::Stats {
                seq: s.seq,
                files: s.files,
                folders: s.folders,
                deleted: s.deleted,
                live_bytes: s.live_bytes,
                stored_bytes: s.stored_bytes,
                devices: s.devices,
            })
        }
        UiCommand::Rekey { password, remember } => start_rekey(cx, password, remember).await,
        UiCommand::Skipped => Ok(UiResult::Skipped {
            files: cx.with(|s| {
                s.index
                    .skipped
                    .iter()
                    .map(|(k, f)| SkippedView {
                        path: k.clone(),
                        size: f.size,
                    })
                    .collect()
            }),
        }),
        UiCommand::Download { path } => {
            let p = vp(&path)?;
            if !cx.with(|s| s.index.skipped.contains_key(p.as_str())) {
                return Err(SyncError::Http {
                    status: 404,
                    code: "not_skipped".into(),
                    message: format!("{p}: не в списке пропущенных"),
                });
            }
            cx.with_mut(|s| {
                s.download_queue.insert(p.as_str().to_owned());
                s.sync_due = Some(s.now);
            });
            Ok(UiResult::Ok)
        }
        UiCommand::RemoveLocal { path } => remove_local(cx, &path).await,
        UiCommand::Conflicts => Ok(UiResult::Conflicts {
            items: cx.with(|s| s.index.conflicts.clone()),
        }),
        UiCommand::PasswordStrength { password } => Ok(UiResult::Strength {
            bits: crate::crypto::password_strength_bits(&password),
        }),
        UiCommand::CreateRecovery { password } => {
            super::crypto_flow::create_recovery(cx, password).await
        }
        UiCommand::RemoveRecovery { password } => {
            super::crypto_flow::remove_recovery(cx, password).await
        }
        UiCommand::Recover {
            phrase,
            new_password,
            remember,
        } => super::crypto_flow::recover(cx, phrase, new_password, remember).await,
    }
}

/// Откат файла к ревизии. Если текущая версия не отправлена на сервер, она не
/// затирается: старая ревизия ложится рядом копией.
async fn restore_revision(cx: &Ctx, path: &str, rev: u64) -> SyncResult<UiResult> {
    let v = vp(path)?;
    let key = v.as_str().to_owned();
    let sp = cx.with(|s| s.encode_path(&v))?;
    let h = api::history(cx, &sp).await?;
    let Some(r) = h.revisions.into_iter().find(|r| r.rev == rev && !r.deleted) else {
        return Err(SyncError::Http {
            status: 404,
            code: "revision_not_found".into(),
            message: format!("ревизии {rev} нет"),
        });
    };
    let hash =
        Hash::from_slice(&r.hash).ok_or_else(|| SyncError::Protocol("ревизия без хэша".into()))?;
    let f = cx.with(|s| s.index.files.get(&key).cloned());
    let target = match &f {
        Some(f) if f.clean() || f.local.is_none() => key.clone(),
        None => key.clone(),
        Some(_) => unique_copy(cx, &key, &format!("rev {rev}")).await?,
    };
    let expect = match f.as_ref().and_then(|f| f.local) {
        Some(l) if target == key => Expect::Stat {
            size: l.size,
            mtime: l.mtime,
        },
        _ => Expect::Absent,
    };
    let written = if r.size <= SMALL_BLOB {
        let plain = fetch_small(cx, &hash).await?;
        cx.write(&target, plain, expect).await?.is_some()
    } else {
        download_to(cx, &target, &hash, r.size, expect)
            .await?
            .is_some()
    };
    if !written {
        return Err(SyncError::Http {
            status: 409,
            code: "file_changed".into(),
            message: "файл изменился, повторите".into(),
        });
    }
    cx.with_mut(|s| {
        s.dirty.insert(target.clone());
        s.sync_due = Some(s.now);
    });
    Ok(UiResult::Ok)
}

/// Решение пользователя по конфликту. До решения ничего не удалялось; после —
/// убранная версия уходит в корзину (и остаётся в истории на сервере).
pub(crate) async fn resolve_choice(cx: &Ctx, id: u64, choice: ConflictChoice) -> SyncResult<()> {
    let rec = cx.with_mut(|s| {
        let pos = s.index.conflicts.iter().position(|c| c.id == id)?;
        Some(s.index.conflicts.remove(pos))
    });
    let Some(rec) = rec else { return Ok(()) };
    match choice {
        ConflictChoice::KeepBoth => {}
        ConflictChoice::KeepServer => {
            cx.trash(&rec.copy, Expect::Any).await?;
        }
        ConflictChoice::KeepMine => {
            let meta = cx.stat(&rec.copy).await?;
            if let Some(meta) = meta {
                if meta.size <= SMALL_BLOB {
                    if let Some(data) = cx.read(&rec.copy, 0, None).await? {
                        cx.write(&rec.path, data, Expect::Any).await?;
                        cx.trash(&rec.copy, Expect::Any).await?;
                    }
                } else {
                    cx.trash(&rec.path, Expect::Any).await?;
                    cx.rename(&rec.copy, &rec.path).await?;
                    cx.with_mut(|s| {
                        if let Some(f) = s.index.files.remove(&rec.copy) {
                            let e = s
                                .index
                                .files
                                .entry(rec.path.clone())
                                .or_insert_with(FileState::default);
                            e.local = f.local;
                        }
                    });
                }
            }
        }
    }
    cx.with_mut(|s| {
        s.dirty.insert(rec.path.clone());
        s.dirty.insert(rec.copy.clone());
        s.sync_due = Some(s.now);
    });
    cx.save().await
}
