//! Один цикл синка: скан → (проверка режима) → push → pull → push.

use super::api;
use super::crypto_flow::{self, check_state, verify_keys};
use super::ctx::{Ctx, SyncError, SyncResult};
use super::pull::pull;
use super::push::push;
use super::scan::scan;
use super::types::{LogLevel, Notice, SyncState};

/// Возвращает, изменилось ли что-нибудь (для адаптивного опроса).
pub(crate) async fn run_cycle(cx: &Ctx) -> SyncResult<bool> {
    cx.set_status(|st| {
        st.state = SyncState::Syncing;
        st.reason = None;
        st.done = 0;
        st.total = 0;
    });
    let mut changed = false;
    for _attempt in 0..4 {
        match one_pass(cx).await {
            Ok(c) => {
                changed |= c;
                return Ok(changed);
            }
            Err(SyncError::Restart) => {
                changed = true;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(changed)
}

async fn one_pass(cx: &Ctx) -> SyncResult<bool> {
    if cx.with(|s| s.encrypted() && s.keys.is_none()) {
        return Err(SyncError::Paused("need_password".into()));
    }
    if cx.with(|s| s.keys.is_some()) {
        verify_keys(cx).await?;
    }
    // Прерванная миграция на шифрование продолжается первой.
    if cx.with(|s| s.index.migration.is_some()) {
        crypto_flow::migrate(cx).await?;
    }
    scan(cx).await?;
    let mut changed = false;
    for _round in 0..3 {
        if cx.with(|s| s.index.pending_count() > 0 && !s.read_only) {
            // Перед отправкой — режим vault'а (не слать открытый текст в зашифрованный).
            let peek = api::changes(cx, cx.with(|s| s.index.last_seq), 1).await?;
            if let Some(vs) = &peek.vault {
                check_state(cx, vs).await?;
            }
            if !cx.with(|s| s.read_only) {
                match push(cx).await {
                    Ok(c) => changed |= c,
                    // Старый сервер не сообщает режим в VaultState: узнали по отказу.
                    Err(SyncError::ReadOnly) => {
                        cx.log(
                            LogLevel::Info,
                            "устройство только для чтения: отправка отключена",
                        );
                        cx.with_mut(|s| s.set_read_only(None));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        changed |= pull(cx).await?;
        if cx.with(|s| s.index.pending_count() == 0 || s.read_only) {
            break;
        }
    }
    let (read_only, pending) = cx.with(|s| (s.read_only, s.index.pending_count()));
    // Включение шифрования — на актуальном vault'е.
    if let Some((password, remember)) = cx.with_mut(|s| s.enable.take()) {
        if read_only {
            cx.notify(read_only_error());
        } else {
            crypto_flow::enable(cx, password, remember).await?;
        }
    }
    if read_only && pending > 0 && !cx.with(|s| s.read_only_notified) {
        cx.with_mut(|s| s.read_only_notified = true);
        cx.notify(Notice::ReadOnly {
            pending: u32::try_from(pending).unwrap_or(u32::MAX),
        });
    }
    cx.save().await?;
    Ok(changed)
}

/// Отказ в действии, которое меняет vault, на устройстве только для чтения.
pub(crate) fn read_only_error() -> Notice {
    Notice::Error {
        code: "read_only".into(),
        message: SyncError::ReadOnly.to_string(),
    }
}
