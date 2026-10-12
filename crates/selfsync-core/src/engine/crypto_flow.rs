//! Ключ vault'а, пароль, включение шифрования на существующем vault'е (раздел 9.4) и
//! смена пароля. Никакого отката к открытому тексту: неверный пароль или
//! расхождение режимов останавливают синк.

use std::collections::{BTreeMap, BTreeSet};

use selfsync_proto::v1 as pb;

use super::api;
use super::ctx::{Ctx, SMALL_BLOB, SyncError, SyncResult};
use super::transfer::{
    Src, download_temp_with, fetch_small_with, prepare_big, prepare_small, upload_big,
};
use super::types::{Action, LogLevel, Notice, Phrase, SyncState, UiResult};
use crate::crypto::{self, CryptoError, MasterKey, RecoveryEntropy, VaultKeys};
use crate::hash::Hash;
use crate::index::{MigrationState, VaultMode};
use crate::path::{VaultPath, path_epoch};

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

/// Переход на новое поколение ключа (смена ключа завершена): базы сбрасываются —
/// записи на сервере теперь другие (новые пути и блобы), содержимое то же.
pub(crate) fn switch_epoch(cx: &Ctx, epoch: u32) {
    rebaseline(cx, false);
    cx.with_mut(|s| {
        s.index.key_epoch = epoch;
        if let Some(k) = s.keys.take() {
            s.keys = Some(k.with_epoch(epoch));
        }
        s.prev_keys = None;
    });
}

fn pause(cx: &Ctx, reason: &str, notice: Option<Notice>) -> SyncError {
    let first = cx.with_mut(|s| {
        let first = s.paused.as_deref() != Some(reason);
        s.paused = Some(reason.to_owned());
        first
    });
    if first && let Some(n) = notice {
        cx.notify(n);
    }
    SyncError::Paused(reason.to_owned())
}

/// Сверяет режим vault'а на сервере с локальным.
pub(crate) async fn check_state(cx: &Ctx, vs: &pb::VaultState) -> SyncResult<()> {
    cx.note_server_state(vs);
    cx.with_mut(|s| {
        s.set_read_only(Some(vs.read_only));
        // Запись ключа сменилась после проверки: ключ в памяти мог устареть.
        if s.keys_verified && vs.key_version != 0 && s.verified_version != vs.key_version {
            s.keys_verified = false;
        }
    });
    let read_only = cx.with(|s| s.read_only);
    let (mode, has_keys, migrating_here) =
        cx.with(|s| (s.index.mode, s.keys.is_some(), s.index.migration.is_some()));
    match mode {
        VaultMode::Plain => {
            if vs.key_version == 0 || migrating_here {
                return Ok(());
            }
            if vs.migration {
                // Шифрование включают на другом устройстве: запись приостановлена.
                return Err(pause(
                    cx,
                    "encryption_started",
                    Some(Notice::EncryptionStarted),
                ));
            }
            if !has_keys {
                return Err(pause(cx, "need_password", Some(Notice::NeedPassword)));
            }
            verify_keys(cx).await?;
            if plaintext_left(cx).await? {
                if read_only {
                    // Довести чужую миграцию не можем: ждём устройство с записью.
                    return Err(pause(
                        cx,
                        "encryption_started",
                        Some(Notice::EncryptionStarted),
                    ));
                }
                // Ключ записан, а миграцию никто не ведёт (включавшее устройство не
                // получило ответ или было убито до чекпоинта): доводим её сами.
                cx.log(
                    LogLevel::Warn,
                    "шифрование включено не до конца: доводим перезаливку",
                );
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
            cx.log(
                LogLevel::Info,
                "vault зашифрован: переход в зашифрованный режим",
            );
            switch_to_encrypted(cx, vs.key_version);
            cx.save().await?;
            Err(SyncError::Restart)
        }
        VaultMode::Encrypted { key_version } => {
            if vs.key_version == 0 {
                cx.notify(Notice::EncryptionMismatch);
                return Err(SyncError::Blocked("encryption_mismatch".into()));
            }
            if vs.migration && !migrating_here {
                // Ключ меняет другое устройство: записи двух поколений вперемешку,
                // ждём конца (запись ключа уже новая — пароль спросим тогда же).
                cx.with_mut(|s| s.rekey_seen = true);
                return Err(pause(cx, "rekey_started", Some(Notice::RekeyStarted)));
            }
            if !has_keys {
                return Err(pause(cx, "need_password", Some(Notice::NeedPassword)));
            }
            let seen = cx.with_mut(|s| std::mem::take(&mut s.rekey_seen));
            if vs.key_version != key_version || seen {
                // Запись ключа сменилась: пароль (мастер тот же) или смена ключа
                // (мастер новый — запомненный не подойдёт, спросим пароль).
                cx.with_mut(|s| s.keys_verified = false);
                verify_keys(cx).await?;
                cx.with_mut(|s| {
                    s.index.mode = VaultMode::Encrypted {
                        key_version: vs.key_version,
                    }
                });
            }
            if migrating_here {
                return Ok(());
            }
            if !cx.with(|s| s.keys_verified) {
                verify_keys(cx).await?;
            }
            let server_epoch = vs.key_epoch.max(1);
            let (ours, record_epoch, record_prev) = cx.with(|s| {
                (
                    s.index.key_epoch.max(1),
                    s.record_epoch.max(1),
                    s.record_prev,
                )
            });
            if record_epoch > server_epoch {
                // Запись уже нового поколения, данные — ещё прежнего, а маркера нет
                // (иначе ждали бы выше): смену ключа начали и бросили. Ключ у нас —
                // новый, им прежние данные не прочитать; переходить на поколение
                // сервера с ним нельзя — только довести перешифровку (прежний ключ —
                // из записи).
                if read_only {
                    return Err(pause(cx, "rekey_started", Some(Notice::RekeyStarted)));
                }
                cx.log(
                    LogLevel::Warn,
                    "смена ключа не доведена: продолжаем перешифровку",
                );
                cx.with_mut(|s| {
                    s.index.migration = Some(MigrationState {
                        key_version: vs.key_version,
                        to_epoch: record_epoch,
                        takeover: true,
                        ..Default::default()
                    });
                });
                cx.save().await?;
                return Err(SyncError::Restart);
            }
            if server_epoch != ours {
                // Смена ключа завершена на другом устройстве: ключ у нас того же
                // поколения, что данные (проверен по записи) — перейти на него.
                cx.log(
                    LogLevel::Info,
                    format!("ключ шифрования сменён: поколение {server_epoch}, полная сверка"),
                );
                switch_epoch(cx, server_epoch);
                cx.save().await?;
                return Err(SyncError::Restart);
            }
            if record_prev && !read_only {
                // Перешифровка закончена (поколение записи = поколению данных),
                // осталось убрать прежний ключ из записи.
                finish_record(cx).await?;
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
    let recovery = crypto::has_recovery(&vk.record);
    cx.set_status(|st| st.recovery = recovery);
    let epoch = crypto::record_epoch(&vk.record).unwrap_or(1);
    let prev = crypto::has_prev(&vk.record);
    cx.with_mut(|s| {
        s.record_epoch = epoch;
        s.record_prev = prev;
        s.verified_version = vk.version;
    });
    let ok = cx.with(|s| {
        s.master
            .as_ref()
            .map(|m| crypto::verify_master(&vk.record, m))
    });
    match ok {
        Some(Ok(true)) => {
            cx.with_mut(|s| {
                s.keys_verified = true;
                // Ключ этой записи — её поколения (пути шифруются с ним).
                if let Some(k) = s.keys.take() {
                    s.keys = Some(k.with_epoch(epoch));
                }
            });
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

fn set_master(cx: &Ctx, master: MasterKey, remember: bool, verified: bool, epoch: u32) {
    if remember {
        cx.hub.emit(Action::RememberKey {
            key: master.as_bytes().to_vec(),
        });
    }
    cx.with_mut(|s| {
        s.keys = Some(master.derive().with_epoch(epoch));
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
        // Поколение уточнит проверка по записи (verify_keys) до первого запроса.
        let epoch = cx.with(|s| s.index.key_epoch.max(1));
        set_master(cx, MasterKey::from_bytes(b), false, false, epoch);
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
            let epoch = crypto::record_epoch(&vk.record).unwrap_or(1);
            cx.with_mut(|s| {
                s.record_epoch = epoch;
                s.record_prev = crypto::has_prev(&vk.record);
                s.verified_version = vk.version;
            });
            set_master(cx, master, remember, true, epoch);
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
                    s.index.mode = VaultMode::Encrypted {
                        key_version: resp.version,
                    };
                }
            });
            cx.save().await?;
            cx.notify(Notice::PasswordChanged);
            return Ok(());
        }
    }
    Err(SyncError::Protocol(
        "запись ключа постоянно меняется".into(),
    ))
}

fn crypto_err(e: CryptoError) -> SyncError {
    let code = match e {
        CryptoError::WrongPassword => "wrong_password",
        CryptoError::WrongPhrase => "wrong_phrase",
        CryptoError::NoRecovery => "no_recovery",
        _ => "bad_vault_key",
    };
    SyncError::Http {
        status: 400,
        code: code.into(),
        message: e.to_string(),
    }
}

/// Перевыпуск записи ключа с проверкой версии (как смена пароля): `make` строит
/// новую запись из текущей. Возвращает результат `make` для успешной записи.
async fn rewrite_vault_key<T>(
    cx: &Ctx,
    mut make: impl FnMut(&[u8]) -> Result<(Vec<u8>, T), CryptoError>,
) -> SyncResult<T> {
    for _ in 0..3 {
        let vk = api::get_vault_key(cx).await?;
        if vk.record.is_empty() {
            return Err(SyncError::Http {
                status: 400,
                code: "no_vault_key".into(),
                message: "vault не зашифрован".into(),
            });
        }
        let (record, out) = make(&vk.record).map_err(crypto_err)?;
        let recovery = crypto::has_recovery(&record);
        if let Some(resp) = api::put_vault_key(cx, record, vk.version).await? {
            cx.with_mut(|s| {
                if let VaultMode::Encrypted { .. } = s.index.mode {
                    s.index.mode = VaultMode::Encrypted {
                        key_version: resp.version,
                    };
                }
            });
            cx.save().await?;
            cx.set_status(|st| st.recovery = recovery);
            return Ok(out);
        }
    }
    Err(SyncError::Protocol(
        "запись ключа постоянно меняется".into(),
    ))
}

/// Создать фразу восстановления (новая заменяет прежнюю).
pub(crate) async fn create_recovery(cx: &Ctx, password: String) -> SyncResult<UiResult> {
    let kdf = cx.with(|s| s.cfg.kdf);
    let now = cx.now();
    let entropy =
        rewrite_vault_key(cx, |rec| crypto::add_recovery(rec, &password, kdf, now)).await?;
    cx.log(LogLevel::Info, "создана фраза восстановления");
    Ok(UiResult::RecoveryPhrase {
        words: Phrase(entropy.words()),
    })
}

/// Убрать фразу восстановления.
pub(crate) async fn remove_recovery(cx: &Ctx, password: String) -> SyncResult<UiResult> {
    let kdf = cx.with(|s| s.cfg.kdf);
    let now = cx.now();
    rewrite_vault_key(cx, |rec| {
        crypto::remove_recovery(rec, &password, kdf, now).map(|r| (r, ()))
    })
    .await?;
    cx.log(LogLevel::Info, "фраза восстановления удалена");
    Ok(UiResult::Ok)
}

/// Вход по фразе: новый пароль, ключ раскрыт на этом устройстве. Остальные
/// устройства с запомненным ключом продолжают работать (мастер тот же), без
/// него — попросят новый пароль.
pub(crate) async fn recover(
    cx: &Ctx,
    phrase: String,
    new_password: String,
    remember: bool,
) -> SyncResult<UiResult> {
    let entropy = RecoveryEntropy::from_phrase(&phrase).map_err(|e| SyncError::Http {
        status: 400,
        code: "bad_phrase".into(),
        message: e.to_string(),
    })?;
    let kdf = cx.with(|s| s.cfg.kdf);
    let now = cx.now();
    let master = rewrite_vault_key(cx, |rec| {
        crypto::recover(rec, &entropy, &new_password, kdf, now)
    })
    .await?;
    // Поколение уточнит проверка по записи в начале цикла.
    let epoch = cx.with(|s| s.index.key_epoch.max(1));
    set_master(cx, master, remember, false, epoch);
    cx.log(
        LogLevel::Info,
        "доступ восстановлен фразой: задан новый пароль",
    );
    cx.notify(Notice::PasswordChanged);
    cx.with_mut(|s| s.sync_due = Some(s.now));
    Ok(UiResult::Ok)
}

/// Включение шифрования (вызывается из цикла после полной синхронизации).
pub(crate) async fn enable(
    cx: &Ctx,
    password: String,
    remember: bool,
    recovery: bool,
) -> SyncResult<()> {
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
    let entropy = if recovery {
        Some(RecoveryEntropy::generate().map_err(|e| SyncError::Io(e.to_string()))?)
    } else {
        None
    };
    let record = match &entropy {
        Some(e) => crypto::seal_master_key_with_recovery(&master, &password, kdf, cx.now(), e),
        None => crypto::seal_master_key(&master, &password, kdf, cx.now()),
    }
    .map_err(|e| SyncError::Io(e.to_string()))?;
    let Some(resp) = api::put_vault_key(cx, record, 0).await? else {
        return unlock(cx, password, remember).await;
    };
    set_master(cx, master, remember, true, 1);
    cx.with_mut(|s| s.verified_version = resp.version);
    if let Some(e) = entropy {
        // Ровно один раз: ни в индекс, ни в журнал фраза не попадает.
        cx.set_status(|st| st.recovery = true);
        cx.notify(Notice::RecoveryPhrase {
            words: Phrase(e.words()),
        });
    }
    cx.with_mut(|s| {
        s.index.migration = Some(MigrationState {
            key_version: resp.version,
            ..Default::default()
        });
    });
    cx.save().await?;
    api::put_migration(cx, resp.version, 0).await?;
    cx.log(LogLevel::Info, "шифрование включено: перезаливка vault'а");
    migrate(cx).await
}

/// Смена ключа (вызывается из цикла после полной синхронизации): новый мастер-ключ
/// под тем же паролем, прежний — в записи под новым, пока идёт перешифровка.
pub(crate) async fn rekey(cx: &Ctx, password: String, remember: bool) -> SyncResult<()> {
    if !cx.with(|s| s.encrypted()) || cx.with(|s| s.index.migration.is_some()) {
        return Ok(());
    }
    let vk = api::get_vault_key(cx).await?;
    let kdf = cx.with(|s| s.cfg.kdf);
    let rk = match crypto::rekey(&vk.record, &password, kdf, cx.now()) {
        Ok(r) => r,
        Err(CryptoError::WrongPassword) => {
            cx.notify(Notice::WrongPassword);
            return Ok(());
        }
        Err(e) => {
            cx.notify(Notice::Error {
                code: crypto_code(&e).into(),
                message: e.to_string(),
            });
            return Ok(());
        }
    };
    // Ключ в памяти — тот, что в записи (иначе перешифровывать нечем).
    let same = cx.with(|s| {
        s.master
            .as_ref()
            .is_some_and(|m| m.check_value() == rk.old.check_value())
    });
    if !same {
        cx.notify(Notice::Error {
            code: "bad_vault_key".into(),
            message: "ключ на устройстве не совпадает с записью на сервере".into(),
        });
        return Ok(());
    }
    let Some(resp) = api::put_vault_key(cx, rk.record, vk.version).await? else {
        cx.notify(Notice::Error {
            code: "key_changed".into(),
            message: "запись ключа изменилась на другом устройстве — повторите".into(),
        });
        return Ok(());
    };
    let epoch = rk.epoch;
    let old_keys = rk.old.derive().with_epoch(epoch - 1);
    set_master(cx, rk.new, remember, true, epoch);
    cx.with_mut(|s| {
        s.verified_version = resp.version;
        s.prev_keys = Some(old_keys);
        s.record_epoch = epoch;
        s.record_prev = true;
        s.index.migration = Some(MigrationState {
            key_version: resp.version,
            to_epoch: epoch,
            ..Default::default()
        });
    });
    if let Some(e) = rk.phrase {
        // Прежняя фраза больше не открывает vault: новую — один раз.
        cx.set_status(|st| st.recovery = true);
        cx.notify(Notice::RecoveryPhrase {
            words: Phrase(e.words()),
        });
    }
    cx.save().await?;
    api::put_migration(cx, resp.version, epoch).await?;
    cx.log(
        LogLevel::Info,
        format!("ключ шифрования сменён: перешифровка vault'а (поколение {epoch})"),
    );
    migrate(cx).await
}

fn crypto_code(e: &CryptoError) -> &'static str {
    match e {
        CryptoError::WrongPassword => "wrong_password",
        CryptoError::RekeyUnfinished => "rekey_unfinished",
        _ => "bad_vault_key",
    }
}

/// Ключи прежнего поколения для перешифровки: из записи (под новым ключом).
async fn prev_keys(cx: &Ctx, to_epoch: u32) -> SyncResult<VaultKeys> {
    if let Some(k) = cx.with(|s| s.prev_keys.clone()) {
        return Ok(k);
    }
    let vk = api::get_vault_key(cx).await?;
    let master = cx
        .with(|s| s.master.clone())
        .ok_or_else(|| SyncError::Paused("need_password".into()))?;
    match crypto::open_prev(&vk.record, &master) {
        Ok(Some(old)) => {
            let k = old.derive().with_epoch(to_epoch.saturating_sub(1));
            cx.with_mut(|s| s.prev_keys = Some(k.clone()));
            Ok(k)
        }
        Ok(None) => Err(SyncError::Blocked("rekey_no_prev".into())),
        Err(_) => {
            // Ключ в памяти не тот (запомненный старый): спросить пароль.
            Err(pause(cx, "need_password", Some(Notice::NeedPassword)))
        }
    }
}

/// Убрать прежний ключ из записи: перешифровка закончена (purge прошёл).
async fn finish_record(cx: &Ctx) -> SyncResult<()> {
    for _ in 0..3 {
        let vk = api::get_vault_key(cx).await?;
        if !crypto::has_prev(&vk.record) {
            cx.with_mut(|s| s.record_prev = false);
            return Ok(());
        }
        let record = crypto::without_prev(&vk.record).map_err(crypto_err)?;
        if let Some(resp) = api::put_vault_key(cx, record, vk.version).await? {
            cx.with_mut(|s| {
                s.record_prev = false;
                s.verified_version = resp.version;
                if let VaultMode::Encrypted { .. } = s.index.mode {
                    s.index.mode = VaultMode::Encrypted {
                        key_version: resp.version,
                    };
                }
            });
            return Ok(());
        }
    }
    Err(SyncError::Protocol(
        "запись ключа постоянно меняется".into(),
    ))
}

/// Перезаливка открытых записей зашифрованными; продолжается после перезапуска.
pub(crate) async fn migrate(cx: &Ctx) -> SyncResult<()> {
    let Some(state) = cx.with(|s| s.index.migration.clone()) else {
        return Ok(());
    };
    if cx.with(|s| s.keys.is_none()) {
        return Err(pause(cx, "need_password", Some(Notice::NeedPassword)));
    }
    if state.to_epoch > 0 {
        // Перешифровку довело другое устройство, пока нас не было: перейти.
        let vs = api::changes(cx, cx.with(|s| s.index.last_seq), 1)
            .await?
            .vault
            .unwrap_or_default();
        if vs.key_epoch >= state.to_epoch {
            cx.log(LogLevel::Info, "смену ключа довело другое устройство");
            cx.with_mut(|s| s.index.migration = None);
            switch_epoch(cx, vs.key_epoch);
            cx.save().await?;
            return Err(SyncError::Restart);
        }
    }
    // Смена ключа: источник — прежнее поколение (ключ из записи), не открытый текст.
    let src = if state.to_epoch > 0 {
        Some(prev_keys(cx, state.to_epoch).await?)
    } else {
        None
    };
    // Маркер мог не дойти до сервера перед перезапуском — повтор идемпотентен.
    match api::put_migration(cx, state.key_version, state.to_epoch).await {
        Ok(()) => {}
        // Брошенную нами смену ключа подхватило другое устройство — ждём его.
        Err(SyncError::Http { code, .. })
            if state.to_epoch > 0 && code == "migration_in_progress" =>
        {
            return Err(pause(cx, "rekey_started", Some(Notice::RekeyStarted)));
        }
        Err(e) => return Err(e),
    }
    let mut refused = None;
    loop {
        if !cx.with(|s| s.index.migration.as_ref().is_some_and(|m| m.uploaded)) {
            let mut complete = false;
            for _ in 0..5 {
                if upload_all(cx, src.as_ref()).await? {
                    complete = true;
                    break;
                }
            }
            if !complete {
                // Без полной перезаливки purge стёр бы неперезалитое: повтор в
                // следующем цикле.
                return Err(SyncError::Io(
                    "миграция: перезалито не всё, повтор позже".into(),
                ));
            }
        }
        let max_seq = cx.with(|s| s.index.migration.as_ref().map_or(0, |m| m.max_seq));
        if api::purge_plaintext(cx, max_seq).await? {
            break;
        }
        if refused == Some(max_seq) {
            // Перезаливка ничего не добавила, а сервер снова отказал: на нём записи,
            // которые здесь не прочитать. Не крутиться — повтор в следующем цикле.
            return Err(SyncError::Io(
                "миграция: на сервере записи, которые не удалось перешифровать".into(),
            ));
        }
        refused = Some(max_seq);
        // Открытые файлы изменились после снимка: доперезалить.
        cx.with_mut(|s| {
            if let Some(m) = &mut s.index.migration {
                m.uploaded = false;
            }
        });
    }
    api::delete_migration(cx).await?;
    if state.to_epoch > 0 {
        finish_record(cx).await?;
        switch_epoch(cx, state.to_epoch);
        cx.with_mut(|s| s.index.migration = None);
        cx.save().await?;
        cx.notify(Notice::RekeyDone);
        cx.log(LogLevel::Info, "смена ключа завершена");
        return Err(SyncError::Restart);
    }
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
        if resp
            .entries
            .iter()
            .any(|e| !e.deleted && e.path.as_ref().is_some_and(|p| !p.encrypted))
        {
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
async fn upload_all(cx: &Ctx, src: Option<&VaultKeys>) -> SyncResult<bool> {
    let keys = cx
        .with(|s| s.keys.clone())
        .ok_or_else(|| SyncError::Paused("need_password".into()))?;
    let to_epoch = cx.with(|s| s.index.migration.as_ref().map_or(0, |m| m.to_epoch));
    // Все открытые живые записи сервера (не только локальные файлы: у других устройств
    // могут быть свои исключения) и последние зашифрованные записи тех же путей.
    let mut cursor = 0;
    let mut plain: BTreeMap<String, pb::Entry> = BTreeMap::new();
    let mut encrypted: BTreeMap<String, pb::Entry> = BTreeMap::new();
    loop {
        let resp = api::changes(cx, cursor, 1000).await?;
        for e in resp.entries {
            let Some(p) = e.path.as_ref() else { continue };
            // Целевые записи — зашифрованные новым ключом; исходные — открытые (при
            // включении шифрования) или прежнего поколения (при смене ключа).
            let target = if to_epoch == 0 {
                p.encrypted
            } else {
                path_epoch(p) == to_epoch
            };
            if target {
                if let Ok(vp) = keys.decrypt_path(p) {
                    encrypted.insert(vp.as_str().to_owned(), e);
                }
                continue;
            }
            // Открытая запись при смене ключа (её мог принять сервер 0.3.0) тоже
            // переносится: иначе purge не прошёл бы никогда.
            let vp = match src {
                _ if !p.encrypted => VaultPath::from_segments(&p.segments).ok(),
                Some(k) => k.decrypt_path(p).ok(),
                None => None,
            };
            let Some(vp) = vp else {
                continue;
            };
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
    let enc_blobs: BTreeSet<Vec<u8>> = encrypted
        .values()
        .filter(|x| !x.deleted && !x.folder)
        .map(|x| x.hash.clone())
        .collect();
    let (todo, takeover): (Vec<(String, pb::Entry)>, bool) = cx.with(|s| {
        let Some(m) = s.index.migration.as_ref() else {
            return (Vec::new(), false);
        };
        (
            plain
                .into_iter()
                .filter(|(k, e)| m.done.get(k) != Some(&e.seq))
                .collect(),
            m.takeover,
        )
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
                    kind: Some(pb::op::Kind::Mkdir(pb::Mkdir {
                        path: Some(enc_path),
                    })),
                });
                meta.push((k.clone(), e.seq));
                continue;
            }
            let Some(h) = Hash::from_slice(&e.hash) else {
                continue;
            };
            let b = if e.size > SMALL_BLOB {
                match reencrypt_big(cx, k, &h, e.size, &keys, src).await? {
                    Some(b) => b,
                    None => {
                        cx.log(
                            LogLevel::Error,
                            format!("миграция: не удалось перезалить {k}"),
                        );
                        complete = false;
                        continue;
                    }
                }
            } else {
                let Some(plain) = plaintext_of(cx, k, &h, src).await? else {
                    // Блоба нет на сервере: спасать нечего, purge не блокируем.
                    cx.log(
                        LogLevel::Error,
                        format!("миграция: блоб {k} пропал на сервере"),
                    );
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
                    let in_history = api::history(cx, &enc_path)
                        .await?
                        .revisions
                        .iter()
                        .any(|r| r.hash == b.hash.to_vec());
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
                    let (date, _) =
                        super::resolve::stamp(cx.now(), cx.with(|s| s.cfg.tz_offset_min));
                    let copy = (1..100)
                        .map(|n| {
                            if n == 1 {
                                format!("plaintext {date}")
                            } else {
                                format!("plaintext {date} {n}")
                            }
                        })
                        .map(|l| vp.with_suffix(&l).as_str().to_owned())
                        .find(|c| !taken.contains(c))
                        .ok_or_else(|| {
                            SyncError::Io(format!("миграция: нет свободного имени для копии {k}"))
                        })?;
                    cx.log(
                        LogLevel::Warn,
                        format!("миграция: открытая версия {k} сохранена копией {copy}"),
                    );
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
async fn reencrypt_big(
    cx: &Ctx,
    key: &str,
    h: &Hash,
    size: u64,
    keys: &crate::crypto::VaultKeys,
    src: Option<&VaultKeys>,
) -> SyncResult<Option<super::transfer::PreparedBlob>> {
    let local = cx.with(|s| {
        s.index
            .files
            .get(key)
            .filter(|f| f.clean() && f.base_blob == Some(*h))
            .and_then(|f| f.local)
    });
    let (temp, obs) = match local {
        Some(obs) => (None, obs),
        None => {
            let (t, obs) = download_temp_with(cx, h, size, src.cloned()).await?;
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
    if !api::blobs_missing(cx, &[b.hash]).await?.is_empty()
        && !upload_big(cx, key, src, &obs, &b, Some(keys)).await?
    {
        return Ok(None);
    }
    if let Some(t) = temp {
        cx.delete_temp(&t).await?;
    }
    Ok(Some(b))
}

/// Открытый текст записи: из локального файла, если он совпадает, иначе с сервера.
async fn plaintext_of(
    cx: &Ctx,
    key: &str,
    h: &Hash,
    src: Option<&VaultKeys>,
) -> SyncResult<Option<Vec<u8>>> {
    let local = cx.with(|s| {
        s.index
            .files
            .get(key)
            .filter(|f| f.clean() && f.base_blob == Some(*h))
            .and_then(|f| f.base_plain)
    });
    if let Some(plain_hash) = local
        && let Some(d) = cx.read(key, 0, None).await?
        && Hash::of(&d) == plain_hash
    {
        return Ok(Some(d));
    }
    match fetch_small_with(cx, h, src.cloned()).await {
        Ok(d) => Ok(Some(d)),
        Err(SyncError::BlobGone) => Ok(None),
        Err(e) => Err(e),
    }
}
