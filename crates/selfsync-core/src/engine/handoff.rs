//! Передача ключа vault'а по коду подключения (протокол 1.4): шаги источника и
//! нового устройства поверх [`crate::handoff`]. Интерфейс опрашивает их командами,
//! пока не придёт `done` (или `sent`). Сбой сети и ответы 5xx — не повод бросать
//! обмен: состояние остаётся, ответ — тот же шаг с `offline`, интерфейс повторяет
//! реже. Обмен забывается на отказе по существу (подмена, неверный ключ, обмен
//! отменён или истёк — 404) и по явной отмене; тогда новое устройство вводит
//! пароль как раньше.

use super::api;
use super::crypto_flow::{self, set_master};
use super::ctx::{Ctx, SyncError, SyncResult};
use super::types::{HandoffStep, LogLevel, UiResult};
use crate::crypto::{self, MasterKey};
use crate::handoff::{self, Ephemeral, LINK_SECRET_LEN, Labels};
use selfsync_proto::MINOR_HANDOFF;

/// Обмен, где это устройство — источник.
#[derive(Debug, Clone)]
pub(crate) struct SourceHandoff {
    eph: Ephemeral,
    /// Секрет из QR: MAC им — подтверждение без сверки цифр.
    link_secret: [u8; LINK_SECRET_LEN],
    /// Ключ нового устройства, каким его увидели впервые.
    joiner_pk: Option<Vec<u8>>,
    /// Последний шаг: имя нового устройства, цифры и MAC сошёлся — его же
    /// показывает интерфейс, пока сервер недоступен.
    name: String,
    sas: Option<String>,
    qr: bool,
    revealed: bool,
    /// Подключившееся устройство (0 — ещё нет): его предлагают отозвать.
    device: u32,
    /// Обёртку уже отправляли: если потом обмена на сервере нет (404), новое
    /// устройство её забрало и закрыло обмен — ключ отправлен.
    wrap_sent: bool,
}

/// Обмен, где это устройство — новое.
#[derive(Debug, Clone)]
pub(crate) struct JoinerHandoff {
    eph: Ephemeral,
    commit: Vec<u8>,
    /// Обязательство сверено по `h` из QR: ключ источника подлинный, сверка
    /// цифр здесь не нужна. Иначе ключ принимается только после неё.
    by_qr: bool,
    /// MAC ключа обмена секретом из QR (пусто — без QR).
    mac: Vec<u8>,
    /// Свой ключ на сервере (иначе отправить при следующем опросе).
    pk_sent: bool,
    /// Цифры, когда ключ источника уже пришёл.
    sas: Option<String>,
}

fn fail<T>(code: &str, message: &str) -> SyncResult<T> {
    Err(SyncError::Http {
        status: 409,
        code: code.into(),
        message: message.into(),
    })
}

/// Сбой связи, а не отказ: сеть, таймаут, 5xx, лимитер, ответ не разобран (прокси).
fn transient(e: &SyncError) -> bool {
    matches!(
        e,
        SyncError::Network(_)
            | SyncError::Server { .. }
            | SyncError::RateLimited { .. }
            | SyncError::Protocol(_)
    )
}

/// Обмена на сервере нет: отменён другой стороной, истёк или закрыт.
fn gone(e: &SyncError) -> bool {
    matches!(e, SyncError::Http { status: 404, .. })
}

fn result(step: HandoffStep, name: String, sas: Option<String>, qr: bool) -> UiResult {
    UiResult::Handoff {
        step,
        name,
        sas,
        qr,
        offline: false,
        device: 0,
    }
}

/// Тот же шаг, что был, с пометкой «нет связи, повторяем».
fn offline(sas: Option<String>, name: String, qr: bool) -> UiResult {
    UiResult::Handoff {
        step: if sas.is_some() {
            HandoffStep::Confirm
        } else {
            HandoffStep::Waiting
        },
        name,
        sas,
        qr,
        offline: true,
        device: 0,
    }
}

/// Источник для нового кода: обязательство и секрет для QR, если ключ есть и
/// сервер умеет обмен. Иначе — `None`: код как раньше, без ключа.
pub(crate) async fn offer(
    cx: &Ctx,
    server_minor: u32,
) -> SyncResult<Option<(Vec<u8>, SourceHandoff)>> {
    if server_minor < MINOR_HANDOFF || cx.with(|s| s.master.is_none()) {
        return Ok(None);
    }
    // Запомненный ключ (CLI вне синка) ещё не сверен: чужой ключ не предлагать.
    if crypto_flow::verify_keys(cx).await.is_err() || cx.with(|s| s.master.is_none()) {
        return Ok(None);
    }
    let eph = Ephemeral::generate().map_err(|e| SyncError::Io(e.to_string()))?;
    let link_secret = crypto::random_bytes().map_err(|e| SyncError::Io(e.to_string()))?;
    Ok(Some((
        handoff::commit(eph.public()).to_vec(),
        SourceHandoff {
            eph,
            link_secret,
            joiner_pk: None,
            name: String::new(),
            sas: None,
            qr: false,
            revealed: false,
            device: 0,
            wrap_sent: false,
        },
    )))
}

/// Секрет для ссылки (`m=` в QR), base64url.
pub(crate) fn link_secret(h: &SourceHandoff) -> String {
    api::base64url(&h.link_secret)
}

/// Префикс обязательства для ссылки (`h=` в QR), base64url.
pub(crate) fn link_check(commit: &[u8]) -> String {
    api::base64url(&handoff::commit_check(commit))
}

/// Шаг источника. `confirm` — цифры шага `confirm`, на которых пользователь
/// согласился отдать ключ (при `qr` — сверять их не обязательно).
pub(crate) async fn source_step(
    cx: &Ctx,
    code: String,
    confirm: Option<String>,
) -> SyncResult<UiResult> {
    let Some(mut h) = cx.with(|s| s.handoffs.get(&code).cloned()) else {
        return fail(
            "no_handoff",
            "обмен ключом по этому коду не начат или закончен",
        );
    };
    let res = match source_advance(cx, &code, &mut h, confirm).await {
        Err(e) if transient(&e) => Ok(offline(h.sas.clone(), h.name.clone(), h.qr)),
        // Обёртку отправляли, а обмена уже нет: новое устройство её забрало.
        Err(e) if gone(&e) && h.wrap_sent => {
            Ok(result(HandoffStep::Sent, h.name.clone(), None, h.qr))
        }
        Err(e) if gone(&e) => fail(
            "handoff_gone",
            "обмен прерван на новом устройстве или истёк",
        ),
        r => r,
    };
    let mut res = res;
    if let Ok(UiResult::Handoff { device, .. }) = &mut res {
        *device = h.device;
    }
    // Подмена: обмен закрыть на сервере, чтобы новое устройство не ждало.
    if matches!(
        res,
        Ok(UiResult::Handoff {
            step: HandoffStep::Rejected,
            ..
        })
    ) {
        let _ = api::handoff_source_cancel(cx, code.clone()).await;
    }
    let ongoing = matches!(
        res,
        Ok(UiResult::Handoff {
            step: HandoffStep::Waiting | HandoffStep::Confirm,
            ..
        })
    );
    cx.with_mut(|s| {
        if ongoing {
            s.handoffs.insert(code.clone(), h);
        } else {
            s.handoffs.remove(&code);
        }
    });
    res
}

async fn source_advance(
    cx: &Ctx,
    code: &str,
    h: &mut SourceHandoff,
    confirm: Option<String>,
) -> SyncResult<UiResult> {
    let labels = Labels::selfsync();
    // Согласие на уже показанных цифрах — сразу обёртка, без лишнего опроса.
    if let (Some(b), Some(sas)) = (h.joiner_pk.clone(), h.sas.clone())
        && h.revealed
        && confirm.as_deref() == Some(sas.as_str())
    {
        return send_wrap(cx, code, h, &b).await;
    }
    let st = api::handoff_source(cx, code.to_owned(), vec![], vec![]).await?;
    h.name = st.name.clone();
    h.device = st.device_id;
    if st.delivered {
        return Ok(result(HandoffStep::Done, st.name, None, h.qr));
    }
    if st.pk.is_empty() {
        return Ok(result(HandoffStep::Waiting, st.name, None, false));
    }
    // Сервер пишет ключ один раз; другой ключ при следующем опросе — подмена.
    match &h.joiner_pk {
        Some(b) if *b != st.pk => {
            cx.log(
                LogLevel::Warn,
                "ключ нового устройства сменился — обмен прерван",
            );
            return Ok(result(HandoffStep::Rejected, st.name, None, false));
        }
        Some(_) => {}
        None => {
            if !handoff::valid_public(&st.pk) {
                return fail("bad_handoff_key", "неверный ключ нового устройства");
            }
            h.joiner_pk = Some(st.pk.clone());
        }
    }
    // Свой ключ — только после чужого: сервер не может подобрать ключ под цифры.
    if !h.revealed {
        api::handoff_source(cx, code.to_owned(), h.eph.public().to_vec(), vec![]).await?;
        h.revealed = true;
    }
    let sas = handoff::sas_digits(&labels, h.eph.public(), &st.pk);
    // MAC из QR снимает сверку цифр, но не согласие: кто сфотографировал QR и
    // успел первым, получил бы ключ сам. Пользователь видит имя и подтверждает.
    let qr = !st.mac.is_empty();
    if qr && !handoff::check_mac(&h.link_secret, &st.pk, &st.mac) {
        cx.log(
            LogLevel::Warn,
            "ключ нового устройства не совпал с QR-кодом — обмен прерван",
        );
        return Ok(result(HandoffStep::Rejected, st.name, None, false));
    }
    h.sas = Some(sas.clone());
    h.qr = qr;
    if confirm.as_deref() != Some(sas.as_str()) {
        return Ok(result(HandoffStep::Confirm, st.name, Some(sas), qr));
    }
    send_wrap(cx, code, h, &st.pk).await
}

async fn send_wrap(
    cx: &Ctx,
    code: &str,
    h: &mut SourceHandoff,
    joiner: &[u8],
) -> SyncResult<UiResult> {
    let Some(master) = cx.with(|s| s.master.clone()) else {
        return fail("no_key", "ключ шифрования на этом устройстве не раскрыт");
    };
    let wrapped = handoff::wrap(&Labels::selfsync(), &h.eph, joiner, master.as_bytes())
        .map_err(|e| SyncError::Io(e.to_string()))?;
    h.wrap_sent = true;
    let done = api::handoff_source(cx, code.to_owned(), vec![], wrapped).await?;
    if !done.delivered {
        return fail("handoff_not_delivered", "сервер не принял обёртку ключа");
    }
    cx.log(
        LogLevel::Info,
        format!("ключ шифрования передан устройству «{}»", done.name),
    );
    Ok(result(HandoffStep::Done, done.name, None, h.qr))
}

/// Источник отменяет обмен («Не передавать», окно закрыто): сервер его забывает,
/// новое устройство получает 404 и переходит к паролю. Код, по которому ещё не
/// подключились, остаётся рабочим — без передачи ключа.
pub(crate) async fn source_cancel(cx: &Ctx, code: String) -> SyncResult<UiResult> {
    // Обёртку уже пытались отправить: отмена на сервере стёрла бы, возможно, уже
    // доставленный ключ — только забыть здесь.
    if let Some(h) = cx.with_mut(|s| s.handoffs.remove(&code))
        && !h.wrap_sent
    {
        // Сеть недоступна — обмен истечёт сам.
        let _ = api::handoff_source_cancel(cx, code).await;
    }
    Ok(UiResult::Ok)
}

/// Новое устройство: начать обмен (после `Redeem` с обязательством источника).
/// `link_secret` и `check` — `m=` и `h=` из QR; без них ключ примется только после
/// сверки цифр на этом устройстве.
pub(crate) async fn receive_start(
    cx: &Ctx,
    commit: Vec<u8>,
    link_secret: Option<String>,
    check: Option<String>,
) -> SyncResult<UiResult> {
    if commit.len() != 32 {
        return fail("bad_commit", "обязательство источника — 32 байта");
    }
    let nonempty = |v: Option<String>| v.map(|x| x.trim().to_owned()).filter(|x| !x.is_empty());
    let by_qr = match nonempty(check) {
        None => false,
        Some(h) => {
            let ok =
                api::base64url_decode(&h).is_some_and(|h| handoff::check_commit_check(&h, &commit));
            if !ok {
                // Обязательство от сервера не то, что показал QR источника: сервер
                // (или кто-то между) подставил свой ключ. Ключ на сервер не уходит.
                let _ = api::handoff_joiner_delete(cx).await;
                return fail(
                    "handoff_commit_mismatch",
                    "обязательство источника не совпало с QR-кодом — возможна подмена, обмен прерван",
                );
            }
            true
        }
    };
    let eph = Ephemeral::generate().map_err(|e| SyncError::Io(e.to_string()))?;
    let mac = match nonempty(link_secret) {
        None => vec![],
        Some(m) => match api::base64url_decode(&m) {
            Some(m) if m.len() == LINK_SECRET_LEN => handoff::link_mac(&m, eph.public()).to_vec(),
            // Испорченный секрет — не повод отказаться: сверка цифр.
            _ => vec![],
        },
    };
    cx.with_mut(|s| {
        s.receiving = Some(JoinerHandoff {
            eph,
            commit,
            by_qr,
            mac,
            pk_sent: false,
            sas: None,
        })
    });
    // Первый шаг — как опрос: при сбое сети ключ уйдёт при следующем.
    receive_poll(cx, None).await
}

/// Новое устройство: опрос. `confirm` — ключ источника пришёл, цифры готовы (без
/// `qr` ключ примется, только когда пользователь их сверит: `confirm` команды —
/// эти цифры); `done` — ключ раскрыт и запомнен, обмен на сервере закрыт.
pub(crate) async fn receive_poll(cx: &Ctx, confirm: Option<String>) -> SyncResult<UiResult> {
    let Some(mut h) = cx.with(|s| s.receiving.clone()) else {
        return fail("no_handoff", "обмен ключом не начат");
    };
    let res = match receive_advance(cx, &mut h, confirm).await {
        Err(e) if transient(&e) => Ok(offline(h.sas.clone(), String::new(), h.by_qr)),
        Err(e) if gone(&e) => fail(
            "handoff_gone",
            "обмен отменён на устройстве с кодом или истёк — введите пароль",
        ),
        r => r,
    };
    let ongoing = matches!(
        res,
        Ok(UiResult::Handoff {
            step: HandoffStep::Waiting | HandoffStep::Confirm,
            ..
        })
    );
    if ongoing {
        cx.with_mut(|s| s.receiving = Some(h));
    } else {
        cx.with_mut(|s| s.receiving = None);
        // Получили или отказались — обмен на сервере больше не нужен.
        let _ = api::handoff_joiner_delete(cx).await;
    }
    res
}

async fn receive_advance(
    cx: &Ctx,
    h: &mut JoinerHandoff,
    confirm: Option<String>,
) -> SyncResult<UiResult> {
    let labels = Labels::selfsync();
    if !h.pk_sent {
        match api::handoff_joiner_put(cx, h.eph.public().to_vec(), h.mac.clone()).await {
            // PUT может только это устройство: «уже записан» — наш же запрос, ответ
            // на который потерялся.
            Ok(()) | Err(SyncError::Http { status: 409, .. }) => {}
            Err(e) => return Err(e),
        }
        h.pk_sent = true;
    }
    let st = api::handoff_joiner_get(cx).await?;
    if st.pk.is_empty() {
        return Ok(result(HandoffStep::Waiting, String::new(), None, h.by_qr));
    }
    if !handoff::check_commit(&st.pk, &h.commit) {
        return fail(
            "handoff_commit_mismatch",
            "ключ источника не совпал с обязательством — возможна подмена, обмен прерван",
        );
    }
    let sas = handoff::sas_digits(&labels, &st.pk, h.eph.public());
    h.sas = Some(sas.clone());
    // Без `h` ключ источника пришёл только от сервера: им мог оказаться ключ
    // сервера. Ключ не открывается и не запоминается, пока цифры не сверены здесь.
    let trusted = h.by_qr || confirm.as_deref() == Some(sas.as_str());
    if st.wrapped.is_empty() || !trusted {
        return Ok(result(
            HandoffStep::Confirm,
            String::new(),
            Some(sas),
            h.by_qr,
        ));
    }
    let key = handoff::unwrap(&labels, &h.eph, &st.pk, &st.wrapped)
        .map_err(|_| SyncError::Corrupt("обёртка ключа не открывается".into()))?;
    let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) else {
        return fail("bad_handoff_key", "в обёртке не ключ vault'а");
    };
    let master = MasterKey::from_bytes(key);
    // Ключ пришёл через сервер: принять его, только если он открывает vault.
    let vk = api::get_vault_key(cx).await?;
    if vk.record.is_empty() {
        return fail("no_vault_key", "vault не зашифрован");
    }
    if !crypto::verify_master(&vk.record, &master).unwrap_or(false) {
        return fail("handoff_wrong_key", "переданный ключ не подходит к vault'у");
    }
    let epoch = crypto::record_epoch(&vk.record).unwrap_or(1);
    cx.with_mut(|s| {
        s.record_epoch = epoch;
        s.record_prev = crypto::has_prev(&vk.record);
        s.verified_version = vk.version;
    });
    set_master(cx, master, true, true, epoch);
    cx.log(
        LogLevel::Info,
        "ключ шифрования получен от другого устройства",
    );
    Ok(result(HandoffStep::Done, String::new(), Some(sas), h.by_qr))
}
