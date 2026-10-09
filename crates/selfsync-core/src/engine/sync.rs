//! Один цикл синка: скан → (проверка режима) → push → pull → push.

use super::api;
use super::crypto_flow::{self, check_state, verify_keys};
use super::ctx::{Ctx, SyncError, SyncResult};
use super::pull::pull;
use super::push::push;
use super::scan::scan;
use super::types::SyncState;

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
        if cx.with(|s| s.index.pending_count()) > 0 {
            // Перед отправкой — режим vault'а (не слать открытый текст в зашифрованный).
            let peek = api::changes(cx, cx.with(|s| s.index.last_seq), 1).await?;
            if let Some(vs) = &peek.vault {
                check_state(cx, vs).await?;
            }
            changed |= push(cx).await?;
        }
        changed |= pull(cx).await?;
        if cx.with(|s| s.index.pending_count()) == 0 {
            break;
        }
    }
    // Включение шифрования — на актуальном vault'е.
    if let Some((password, remember)) = cx.with_mut(|s| s.enable.take()) {
        crypto_flow::enable(cx, password, remember).await?;
    }
    cx.save().await?;
    Ok(changed)
}
