//! Ключ vault'а, пароль, включение шифрования на существующем vault'е (раздел 9.4) и
//! смена пароля. Никакого отката к открытому тексту: неверный пароль или
//! расхождение режимов останавливают синк.

use std::collections::{BTreeMap, BTreeSet};

use notesync_proto::v1 as pb;

use super::api;
use super::ctx::{Ctx, SMALL_BLOB, SyncError, SyncResult};
use super::transfer::{Src, download_temp, fetch_small, prepare_big, prepare_small, upload_big};
use super::types::{Action, LogLevel, Notice, SyncState};
use crate::crypto::{self, CryptoError, MasterKey};
use crate::hash::Hash;
use crate::index::{MigrationState, VaultMode};
use crate::path::VaultPath;

/// Сбрасывает базы индекса: следующая синхронизация сверит каждый файл с сервером
/// заново (совпадающие — no-op на сервере, различающиеся — через разрешение
/// расхождений с прежней базой открытого текста).
///
/// `skip_tombstones`: не применять удаления в первом проходе (сервер восстановлен из
/// бэкапа — его tombstone'ы могут быть старше наших версий).
pub(crate) fn rebaseline(cx: &Ctx, skip_tombstones: bool) {
    cx.with_mut(|s| {
        for f in s.index.files.values_mut() {
            f.base_rev = 0;
            f.base_blob = None;
            f.transfer = None;
            f.rejected = None;
        }
        s.index.last_seq = 0;
        s.index.rebaselined = true;
        s.index.rebaseline_seen.clear();
        if skip_tombstones {
            s.index.initial_done = false;
            s.index.rewound = true;
        }
        s.need_full_scan = true;
    });
}

/// Переход индекса в зашифрованный режим.
pub(crate) fn switch_to_encrypted(cx: &Ctx, key_version: u64) {
    rebaseline(cx, false);
    cx.with_mut(|s| {
        s.index.mode = VaultMode::Encrypted { key_version };
        s.index.migration = None;
    });
}

fn pause(cx: &Ctx, reason: &str, notice: Option<Notice>) -> SyncError {
    let first = cx.with_mut(|s| {
        let first = s.paused.as_deref() != Some(reason);
        s.paused = Some(reason.to_owned());
        first
    });
    if first {
        if let Some(n) = notice {
            cx.notify(n);
        }
    }
    SyncError::Paused(reason.to_owned())
}

/// Сверяет режим vault'а на сервере с локальным.
pub(crate) async fn check_state(cx: &Ctx, vs: &pb::VaultState) -> SyncResult<()> {
    cx.with_mut(|s| s.server_state = Some(vs.clone()));
    let (mode, has_keys, migrating_here) = cx.with(|s| (s.index.mode, s.keys.is_some(), s.index.migration.is_some()));
    match mode {
        VaultMode::Plain => {
            if vs.key_version == 0 || migrating_here {
                return Ok(());
            }
            if vs.migration {
                // Шифрование включают на другом устройстве: запись приостановлена.
                return Err(pause(cx, "encryption_started", Some(Notice::EncryptionStarted)));
            }
            if !has_keys {
                return Err(pause(cx, "need_password", Some(Notice::NeedPassword)));
            }
            verify_keys(cx).await?;
            if plaintext_left(cx).await? {
                // Ключ записан, а миграцию никто не ведёт (включавшее устройство не
                // получило ответ или было убито до чекпоинта): доводим её сами.
                cx.log(LogLevel::Warn, "шифрование включено не до конца: доводим перезаливку");
                cx.with_mut(|s| {
                    s.index.migration = Some(MigrationState {
                        key_version: vs.key_version,
                        takeover: true,
                        ..Default::default()
                    });
                });
                cx.save().await?;
                return Err(SyncError::Restart);
            }
            cx.log(LogLevel::Info, "vault зашифрован: переход в зашифрованный режим");
            switch_to_encrypted(cx, vs.key_version);
            cx.save().await?;
            Err(SyncError::Restart)
        }
        VaultMode::Encrypted { key_version } => {
            if vs.key_version == 0 {
                cx.notify(Notice::EncryptionMismatch);
                return Err(SyncError::Blocked("encryption_mismatch".into()));
            }
            if !has_keys {
                return Err(pause(cx, "need_password", Some(Notice::NeedPassword)));
            }
            if vs.key_version != key_version {
                // Пароль сменили на другом устройстве: мастер-ключ тот же — проверить.
                cx.with_mut(|s| s.keys_verified = false);
                verify_keys(cx).await?;
                cx.with_mut(|s| s.index.mode = VaultMode::Encrypted { key_version: vs.key_version });
            }
            Ok(())
        }
    }
}

/// Сверяет ключ в памяти с записью на сервере (один раз).
pub(crate) async fn verify_keys(cx: &Ctx) -> SyncResult<()> {
    if cx.with(|s| s.keys_verified || s.master.is_none()) {
        return Ok(());
    }
    let vk = api::get_vault_key(cx).await?;
    if vk.record.is_empty() {
        return Ok(());
    }
    let ok = cx.with(|s| s.master.as_ref().map(|m| crypto::verify_master(&vk.record, m)));
    match ok {
        Some(Ok(true)) => {
            cx.with_mut(|s| s.keys_verified = true);
            Ok(())
        }
        _ => {
            // Запомненный ключ не подходит: забыть его и попросить пароль.
            cx.with_mut(|s| {
                s.keys = None;
                s.master = None;
            });
            cx.hub.emit(Action::ForgetKey);
            Err(pause(cx, "need_password", Some(Notice::NeedPassword)))
        }
    }
}

fn set_master(cx: &Ctx, master: MasterKey, remember: bool, verified: bool) {
    if remember {
        cx.hub.emit(Action::RememberKey {
            key: master.as_bytes().to_vec(),
        });
    }
    cx.with_mut(|s| {
        s.keys = Some(master.derive());
        s.master = Some(master);
        s.keys_verified = verified;
        if s.paused.as_deref() == Some("need_password") {
            s.paused = None;
        }
        if s.blocked.as_deref() == Some("wrong_password") {
            s.blocked = None;
        }
    });
}

/// Ключ, запомненный на устройстве (проверяется по записи на сервере в цикле).
pub(crate) fn set_remembered(cx: &Ctx, key: &[u8]) {
    if let Ok(b) = <[u8; 32]>::try_from(key) {
        set_master(cx, MasterKey::from_bytes(b), false, false);
    }
}

/// Пароль от пользователя: раскрыть мастер-ключ.
pub(crate) async fn unlock(cx: &Ctx, password: String, remember: bool) -> SyncResult<()> {
    let vk = api::get_vault_key(cx).await?;
    if vk.record.is_empty() {
        cx.notify(Notice::Error {
            code: "no_vault_key".into(),
            message: "vault не зашифрован".into(),
        });
        return Ok(());
    }
    match crypto::open_master_key(&vk.record, &password) {
        Ok(master) => {
            set_master(cx, master, remember, true);
            cx.log(LogLevel::Info, "ключ шифрования раскрыт");
            cx.with_mut(|s| s.sync_due = Some(s.now));
            Ok(())
        }
        Err(CryptoError::WrongPassword) => {
            cx.with_mut(|s| s.blocked = Some("wrong_password".into()));
            cx.notify(Notice::WrongPassword);
            cx.set_status(|st| {
                st.state = SyncState::Blocked;
                st.reason = Some("wrong_password".into());
            });
            Ok(())
        }
        Err(e) => {
            cx.notify(Notice::Error {
                code: "bad_vault_key".into(),
                message: e.to_string(),
            });
            Ok(())
        }
    }
}

/// Смена пароля: тот же мастер-ключ, новая запись (If-Match текущей версии).
pub(crate) async fn change_password(cx: &Ctx, old: String, new: String) -> SyncResult<()> {
    for _ in 0..3 {
        let vk = api::get_vault_key(cx).await?;
        if vk.record.is_empty() {
            cx.notify(Notice::Error {
                code: "no_vault_key".into(),
                message: "vault не зашифрован".into(),
            });
            return Ok(());
        }
        let kdf = cx.with(|s| s.cfg.kdf);
        let record = match crypto::change_password(&vk.record, &old, &new, kdf, cx.now()) {
            Ok(r) => r,
            Err(CryptoError::WrongPassword) => {
                cx.notify(Notice::WrongPassword);
                return Ok(());
            }
            Err(e) => {
                cx.notify(Notice::Error {
                    code: "bad_vault_key".into(),
                    message: e.to_string(),
                });
                return Ok(());
            }
        };
        if let Some(resp) = api::put_vault_key(cx, record, vk.version).await? {
            cx.with_mut(|s| {
                if let VaultMode::Encrypted { .. } = s.index.mode {
                    s.index.mode = VaultMode::Encrypted { key_version: resp.version };
                }
            });
            cx.save().await?;
            cx.notify(Notice::PasswordChanged);
            return Ok(());
        }
    }
    Err(SyncError::Protocol("запись ключа постоянно меняется".into()))
}

/// Включение шифрования (вызывается из цикла после полной синхронизации).
pub(crate) async fn enable(cx: &Ctx, password: String, remember: bool) -> SyncResult<()> {
    if cx.with(|s| s.encrypted() || s.index.migration.is_some()) {
        return Ok(());
    }
    let vk = api::get_vault_key(cx).await?;
    if !vk.record.is_empty() {
        // Уже включено (другим устройством): тот же пароль раскроет ключ.
        return unlock(cx, password, remember).await;
    }
    let master = MasterKey::generate().map_err(|e| SyncError::Io(e.to_string()))?;
    let kdf = cx.with(|s| s.cfg.kdf);
    let record = crypto::seal_master_key(&master, &password, kdf, cx.now()).map_err(|e| SyncError::Io(e.to_string()))?;
    let Some(resp) = api::put_vault_key(cx, record, 0).await? else {
        return unlock(cx, password, remember).await;
    };
    set_master(cx, master, remember, true);
    cx.with_mut(|s| {
        s.index.migration = Some(MigrationState {
            key_version: resp.version,
            ..Default::default()
        });
    });
    cx.save().await?;
    api::put_migration(cx, resp.version).await?;
    cx.log(LogLevel::Info, "шифрование включено: перезаливка vault'а");
    migrate(cx).await
}

/// Перезаливка открытых записей зашифрованными; продолжается после перезапуска.
pub(crate) async fn migrate(cx: &Ctx) -> SyncResult<()> {
    let Some(state) = cx.with(|s| s.index.migration.clone()) else {
        return Ok(());
    };
    if cx.with(|s| s.keys.is_none()) {
        return Err(pause(cx, "need_password", Some(Notice::NeedPassword)));
    }
    // Маркер мог не дойти до сервера перед перезапуском — повтор идемпотентен.
    api::put_migration(cx, state.key_version).await?;
    loop {
        if !cx.with(|s| s.index.migration.as_ref().is_some_and(|m| m.uploaded)) {
            let mut complete = false;
            for _ in 0..5 {
                if upload_all(cx).await? {
                    complete = true;
                    break;
                }
            }
            if !complete {
                // Без полной перезаливки purge стёр бы неперезалитое: повтор в
                // следующем цикле.
                return Err(SyncError::Io("миграция: перезалито не всё, повтор позже".into()));
            }
        }
        let max_seq = cx.with(|s| s.index.migration.as_ref().map_or(0, |m| m.max_seq));
        if api::purge_plaintext(cx, max_seq).await? {
            break;
        }
        // Открытые файлы изменились после снимка: доперезалить.
        cx.with_mut(|s| {
            if let Some(m) = &mut s.index.migration {
                m.uploaded = false;
            }
        });
    }
    api::delete_migration(cx).await?;
    let kv = state.key_version;
    switch_to_encrypted(cx, kv);
    cx.save().await?;
    cx.notify(Notice::EncryptionEnabled);
    cx.log(LogLevel::Info, "миграция на шифрование завершена");
    Err(SyncError::Restart)
}

/// Есть ли на сервере живые открытые записи.
async fn plaintext_left(cx: &Ctx) -> SyncResult<bool> {
    let mut cursor = 0;
    loop {
        let resp = api::changes(cx, cursor, 1000).await?;
        if resp.entries.iter().any(|e| !e.deleted && e.path.as_ref().is_some_and(|p| !p.encrypted)) {
            return Ok(true);
        }
        cursor = resp.next_seq.max(cursor);
        if !resp.has_more {
            return Ok(false);
        }
    }
}

/// Один проход перезаливки. Возвращает `true`, если перезалито всё: только тогда
/// можно стирать открытые записи (purge снимает всё, что не новее `max_seq`).
async fn upload_all(cx: &Ctx) -> SyncResult<bool> {
    let keys = cx.with(|s| s.keys.clone()).ok_or_else(|| SyncError::Paused("need_password".into()))?;
    // Все открытые живые записи сервера (не только локальные файлы: у других устройств
    // могут быть свои исключения) и последние зашифрованные записи тех же путей.
    let mut cursor = 0;
    let mut plain: BTreeMap<String, pb::Entry> = BTreeMap::new();
    let mut encrypted: BTreeMap<String, pb::Entry> = BTreeMap::new();
    loop {
        let resp = api::changes(cx, cursor, 1000).await?;
        for e in resp.entries {
            let Some(p) = e.path.as_ref() else { continue };
            if p.encrypted {
                if let Ok(vp) = keys.decrypt_path(p) {
                    encrypted.insert(vp.as_str().to_owned(), e);
                }
                continue;
            }
            let Ok(vp) = VaultPath::from_segments(&p.segments) else { continue };
            if e.deleted {
                plain.remove(vp.as_str());
            } else {
                plain.insert(vp.as_str().to_owned(), e);
            }
        }
        cursor = resp.next_seq.max(cursor);
        if !resp.has_more {
            break;
        }
    }
    // Занятые имена (для копий) и блобы живых зашифрованных файлов.
    let mut taken: BTreeSet<String> = plain.keys().chain(encrypted.keys()).cloned().collect();
    let enc_blobs: BTreeSet<Vec<u8>> = encrypted.values().filter(|x| !x.deleted && !x.folder).map(|x| x.hash.clone()).collect();
    let (todo, takeover): (Vec<(String, pb::Entry)>, bool) = cx.with(|s| {
        let Some(m) = s.index.migration.as_ref() else { return (Vec::new(), false) };
        (plain.into_iter().filter(|(k, e)| m.done.get(k) != Some(&e.seq)).collect(), m.takeover)
    });
    let total = u32::try_from(todo.len()).unwrap_or(u32::MAX);
    let mut complete = true;
    let mut n = 0u32;
    for chunk in todo.chunks(50) {
        let mut ops = Vec::new();
        let mut meta: Vec<(String, u64)> = Vec::new();
        for (k, e) in chunk {
            let vp = VaultPath::parse(k).map_err(|er| SyncError::Io(er.to_string()))?;
            let enc_path = keys.encrypt_path(&vp);
            let enc = encrypted.get(k);
            if e.folder {
                if enc.is_some_and(|x| x.folder && !x.deleted) {
                    mark_done(cx, k, e.seq);
                    continue;
                }
                ops.push(pb::Op {
                    kind: Some(pb::op::Kind::Mkdir(pb::Mkdir { path: Some(enc_path) })),
                });
                meta.push((k.clone(), e.seq));
                continue;
            }
            let Some(h) = Hash::from_slice(&e.hash) else { continue };
            let b = if e.size > SMALL_BLOB {
                match reencrypt_big(cx, k, &h, e.size, &keys).await? {
                    Some(b) => b,
                    None => {
                        cx.log(LogLevel::Error, format!("миграция: не удалось перезалить {k}"));
                        complete = false;
                        continue;
                    }
                }
            } else {
                let Some(plain) = plaintext_of(cx, k, &h).await? else {
                    // Блоба нет на сервере: спасать нечего, purge не блокируем.
                    cx.log(LogLevel::Error, format!("миграция: блоб {k} пропал на сервере"));
                    mark_done(cx, k, e.seq);
                    continue;
                };
                let ph = Hash::of(&plain);
                prepare_small(&plain, ph, Some(&keys))
            };
            // Шифрование детерминированное: та же открытая версия даёт тот же блоб.
            if let Some(x) = enc {
                if !x.deleted && x.hash == b.hash.to_vec() {
                    mark_done(cx, k, e.seq);
                    continue;
                }
                // Своя миграция: зашифрованные записи пишем только мы, и раз блоб
                // другой, это копия более старой открытой версии — перезаписать. Чужая
                // брошенная: зашифрованная запись может быть правкой пользователя —
                // главнее та, что записана позже.
                if takeover && x.seq > e.seq {
                    // Открытая версия уже бывала зашифрованной (в истории пути или в
                    // другом файле) — её содержимое сохранено, purge ничего не теряет.
                    let in_history = api::history(cx, &enc_path).await?.revisions.iter().any(|r| r.hash == b.hash.to_vec());
                    if in_history || enc_blobs.contains(&b.hash.to_vec()) {
                        mark_done(cx, k, e.seq);
                        continue;
                    }
                }
            }
            let (put_path, base_rev) = match enc {
                Some(x) if takeover && x.seq > e.seq => {
                    // Обе версии могут нести правки, которых нет в другой: открытая
                    // сохраняется зашифрованной копией рядом.
                    let (date, _) = super::resolve::stamp(cx.now(), cx.with(|s| s.cfg.tz_offset_min));
                    let copy = (1..100)
                        .map(|n| if n == 1 { format!("plaintext {date}") } else { format!("plaintext {date} {n}") })
                        .map(|l| vp.with_suffix(&l).as_str().to_owned())
                        .find(|c| !taken.contains(c))
                        .ok_or_else(|| SyncError::Io(format!("миграция: нет свободного имени для копии {k}")))?;
                    cx.log(LogLevel::Warn, format!("миграция: открытая версия {k} сохранена копией {copy}"));
                    taken.insert(copy.clone());
                    let cp = VaultPath::parse(&copy).map_err(|er| SyncError::Io(er.to_string()))?;
                    (keys.encrypt_path(&cp), 0)
                }
                _ => (enc_path, enc.map_or(0, |x| x.rev)),
            };
            if b.data.is_some() && !api::blobs_missing(cx, &[b.hash]).await?.is_empty() {
                super::transfer::upload_small(cx, &b).await?;
            }
            ops.push(pb::Op {
                kind: Some(pb::op::Kind::Put(pb::Put {
                    path: Some(put_path),
                    base_rev,
                    hash: b.hash.to_vec(),
                    size: b.len,
                    mtime: e.mtime,
                })),
            });
            meta.push((k.clone(), e.seq));
        }
        if ops.is_empty() {
            continue;
        }
        let resp = api::ops(cx, ops).await?;
        for ((k, seq), r) in meta.iter().zip(resp.results) {
            match r.result {
                Some(pb::op_result::Result::Applied(_)) => mark_done(cx, k, *seq),
                other => {
                    // Conflict: зашифрованная запись сменилась после листинга —
                    // разберёмся на следующем проходе.
                    cx.log(LogLevel::Warn, format!("миграция {k}: {other:?}, повтор"));
                    complete = false;
                }
            }
        }
        n += u32::try_from(chunk.len()).unwrap_or(0);
        cx.save().await?;
        cx.notify(Notice::MigrationProgress { done: n, total });
    }
    if complete {
        cx.with_mut(|s| {
            if let Some(m) = &mut s.index.migration {
                m.uploaded = true;
            }
        });
    }
    cx.save().await?;
    Ok(complete)
}

fn mark_done(cx: &Ctx, key: &str, seq: u64) {
    cx.with_mut(|s| {
        if let Some(m) = &mut s.index.migration {
            m.done.insert(key.to_owned(), seq);
            m.max_seq = m.max_seq.max(seq);
        }
    });
}

/// Большой файл: перешифровать потоком — из локальной копии, если она совпадает с
/// сервером, иначе через временный файл.
async fn reencrypt_big(cx: &Ctx, key: &str, h: &Hash, size: u64, keys: &crate::crypto::VaultKeys) -> SyncResult<Option<super::transfer::PreparedBlob>> {
    let local = cx.with(|s| s.index.files.get(key).filter(|f| f.clean() && f.base_blob == Some(*h)).and_then(|f| f.local));
    let (temp, obs) = match local {
        Some(obs) => (None, obs),
        None => {
            let (t, obs) = download_temp(cx, h, size).await?;
            (Some(t), obs)
        }
    };
    let src = match &temp {
        Some(t) => Src::Temp(t),
        None => Src::Vault(key),
    };
    let Some(b) = prepare_big(cx, src, &obs, Some(keys)).await? else {
        return Ok(None);
    };
    if !api::blobs_missing(cx, &[b.hash]).await?.is_empty() && !upload_big(cx, key, src, &obs, &b, Some(keys)).await? {
        return Ok(None);
    }
    if let Some(t) = temp {
        cx.delete_temp(&t).await?;
    }
    Ok(Some(b))
}

/// Открытый текст записи: из локального файла, если он совпадает, иначе с сервера.
async fn plaintext_of(cx: &Ctx, key: &str, h: &Hash) -> SyncResult<Option<Vec<u8>>> {
    let local = cx.with(|s| s.index.files.get(key).filter(|f| f.clean() && f.base_blob == Some(*h)).and_then(|f| f.base_plain));
    if let Some(plain_hash) = local {
        if let Some(d) = cx.read(key, 0, None).await? {
            if Hash::of(&d) == plain_hash {
                return Ok(Some(d));
            }
        }
    }
    match fetch_small(cx, h).await {
        Ok(d) => Ok(Some(d)),
        Err(SyncError::BlobGone) => Ok(None),
        Err(e) => Err(e),
    }
}
