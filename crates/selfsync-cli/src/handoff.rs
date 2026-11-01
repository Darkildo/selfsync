//! Передача ключа шифрования по коду подключения в терминале (протокол 1.4):
//! `join` — устройство с ключом (источник), `connect` — новое. Шаги и криптография —
//! в ядре; здесь опрос, вывод и вопрос «передать ключ?».

use std::time::Duration;

use selfsync_core::engine::{HandoffStep, UiCommand, UiResult};

use crate::runner::Session;

/// Опрос шагов обмена.
pub const POLL: Duration = Duration::from_millis(1500);
/// Сколько новое устройство ждёт ключ (столько же сервер хранит обмен).
pub const WAIT_MS: i64 = 10 * 60 * 1000;

/// Ответ на вопрос «да/нет».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// Ctrl-C во время вопроса.
    Interrupted,
}

/// Чем кончился обмен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    /// Ключ передан (источник) или получен и сохранён (новое устройство).
    Done,
    /// Не передан: отказ, ошибка, срок вышел.
    Refused,
    /// Ctrl-C.
    Interrupted,
}

/// Ввод-вывод обмена: терминал или сценарий теста.
pub trait Prompt {
    fn say(&mut self, text: &str);
    /// Вопрос «да/нет»; по умолчанию — нет.
    fn ask(&mut self, question: &str) -> impl std::future::Future<Output = Answer>;
}

/// Терминал: сообщения — в stderr, ответ — строка из stdin. Ctrl-C во время
/// вопроса прерывает обмен.
pub struct Terminal;

impl Prompt for Terminal {
    fn say(&mut self, text: &str) {
        eprintln!("{text}");
    }

    async fn ask(&mut self, question: &str) -> Answer {
        use std::io::{BufRead, Write};
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        // Чтение stdin блокирует: в отдельном потоке, чтобы Ctrl-C прервал вопрос.
        let line = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).map(|_| line)
        });
        tokio::select! {
            r = line => match r {
                Ok(Ok(l)) if matches!(l.trim().to_lowercase().as_str(), "y" | "yes" | "д" | "да") => {
                    Answer::Yes
                }
                _ => Answer::No,
            },
            _ = tokio::signal::ctrl_c() => {
                eprintln!();
                Answer::Interrupted
            }
        }
    }
}

/// Обмен возможен только с человеком у терминала: без TTY его не предлагают.
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Сервер и код из ссылки подключения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub server: String,
    pub code: String,
    /// `m=` — секрет обмена из QR.
    pub secret: Option<String>,
    /// `h=` — префикс обязательства источника из QR.
    pub check: Option<String>,
}

/// `obsidian://selfsync-connect?server=…&code=…[&m=…]` (QR) или страница сервера
/// `https://сервер/join/<код>`.
pub fn parse_link(s: &str) -> Option<Link> {
    let s = s.trim();
    if let Some(q) = s.strip_prefix("obsidian://selfsync-connect?") {
        let mut server = None;
        let mut code = None;
        let mut secret = None;
        let mut check = None;
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=')?;
            let v = percent_decode(v)?;
            match k {
                "server" => server = Some(v),
                "code" => code = Some(v),
                "m" => secret = Some(v),
                "h" => check = Some(v),
                _ => {}
            }
        }
        return Some(Link {
            server: server?.trim_end_matches('/').to_owned(),
            code: code?,
            secret,
            check,
        });
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        let (base, code) = s.trim_end_matches('/').rsplit_once("/join/")?;
        if code.is_empty() || code.contains(['/', '?', '#']) {
            return None;
        }
        return Some(Link {
            server: base.to_owned(),
            code: code.to_owned(),
            secret: None,
            check: None,
        });
    }
    None
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.bytes() {
        if c.is_ascii_alphanumeric() || b"-._~".contains(&c) {
            out.push(char::from(c));
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Ссылка для QR с секретом обмена `m` и префиксом обязательства `h` — та же, что
/// строит плагин. Через сервер она не проходит: QR ведёт сразу в Obsidian.
pub fn connect_link(server: &str, code: &str, secret: &str, check: &str) -> String {
    format!(
        "obsidian://selfsync-connect?server={}&code={}&m={}&h={}",
        percent_encode(server),
        percent_encode(code),
        percent_encode(secret),
        percent_encode(check)
    )
}

/// Шесть цифр группами: `042 917`.
pub fn format_sas(sas: &str) -> String {
    if sas.len() == 6 {
        format!("{} {}", &sas[..3], &sas[3..])
    } else {
        sas.to_owned()
    }
}

/// QR-код в терминал (полублоки Unicode, по две строки модулей на строку текста;
/// светлые модули рисуются — тёмный фон терминала).
pub fn render_qr(data: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let w = code.width() as isize;
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| -> bool {
        x >= 0 && y >= 0 && x < w && y < w && colors[(y * w + x) as usize] == qrcode::Color::Dark
    };
    let quiet = 2isize;
    let mut out = String::new();
    let mut y = -quiet;
    while y < w + quiet {
        for x in -quiet..w + quiet {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            });
        }
        out.push('\n');
        y += 2;
    }
    Ok(out)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Пауза между шагами; `false` — Ctrl-C.
async fn pause(interval: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(interval) => true,
        _ = tokio::signal::ctrl_c() => false,
    }
}

/// Самая длинная пауза опроса, пока сервер недоступен.
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// Пауза опроса: обычная, а без связи — вдвое дольше прежней (до 15 с).
fn next_pause(cur: Duration, base: Duration, offline: bool, ui: &mut impl Prompt) -> Duration {
    if !offline {
        return base;
    }
    if cur == base {
        ui.say("нет связи с сервером — повторяем…");
    }
    (cur * 2).min(BACKOFF_MAX.max(base))
}

/// Источник: ждать новое устройство по коду и передать ему ключ с согласия
/// пользователя (при коде, введённом руками, — после сверки цифр). Сбой сети — не
/// повод бросать обмен: опрос продолжается реже.
pub async fn give_key(
    sess: &mut Session,
    code: &str,
    deadline_ms: i64,
    interval: Duration,
    ui: &mut impl Prompt,
) -> anyhow::Result<Exchange> {
    let mut confirm: Option<String> = None;
    let mut waiting_said = false;
    let mut wait = interval;
    while now_ms() < deadline_ms {
        let r = sess
            .command(UiCommand::JoinHandoff {
                code: code.to_owned(),
                confirm: confirm.clone(),
            })
            .await?;
        match r {
            UiResult::Handoff {
                step: HandoffStep::Done,
                name,
                ..
            } => {
                ui.say(&format!("ключ шифрования передан «{name}»"));
                return Ok(Exchange::Done);
            }
            UiResult::Handoff {
                step: HandoffStep::Sent,
                name,
                ..
            } => {
                ui.say(&format!(
                    "ключ шифрования мог быть отправлен «{name}» — проверьте на новом \
                     устройстве, что он принят"
                ));
                return Ok(Exchange::Done);
            }
            UiResult::Handoff {
                step: HandoffStep::Rejected,
                name,
                device,
                ..
            } => {
                ui.say(&format!(
                    "ключ «{name}» не совпал с QR-кодом — возможна подмена, ключ не передан"
                ));
                return offer_revoke(sess, &name, device, ui).await;
            }
            UiResult::Handoff {
                step: HandoffStep::Confirm,
                name,
                sas: Some(sas),
                qr,
                offline,
                device,
            } if confirm.as_deref() != Some(sas.as_str()) => {
                let question = if qr {
                    format!(
                        "«{name}» подключилось по QR-коду. Передать ему ключ шифрования? \
                         (если новое устройство показывает цифры {}, сверьте: не совпадают — \
                         отвечайте «нет»)",
                        format_sas(&sas)
                    )
                } else {
                    format!(
                        "«{name}» подключилось по коду. На нём должны быть цифры {}. \
                         Передать ключ шифрования, только если совпадают?",
                        format_sas(&sas)
                    )
                };
                if offline {
                    ui.say("нет связи с сервером — ответ отправится, когда она вернётся");
                }
                match ui.ask(&question).await {
                    Answer::Yes => {}
                    a => {
                        cancel(sess, code).await;
                        if a == Answer::Interrupted {
                            return Ok(Exchange::Interrupted);
                        }
                        ui.say("ключ не передан: на новом устройстве понадобится пароль");
                        return offer_revoke(sess, &name, device, ui).await;
                    }
                }
                confirm = Some(sas);
                // Ответ — сразу, без паузы.
                continue;
            }
            UiResult::Handoff { step, offline, .. } => {
                if step == HandoffStep::Waiting && !waiting_said {
                    ui.say("ждём новое устройство (Ctrl-C — выйти)…");
                    waiting_said = true;
                }
                wait = next_pause(wait, interval, offline, ui);
            }
            UiResult::Error { message, .. } => {
                ui.say(&format!(
                    "ключ не передан: {message}. На новом устройстве можно ввести пароль"
                ));
                return Ok(Exchange::Refused);
            }
            other => anyhow::bail!("неожиданный ответ: {other:?}"),
        }
        if !pause(wait).await {
            cancel(sess, code).await;
            return Ok(Exchange::Interrupted);
        }
    }
    cancel(sess, code).await;
    ui.say("новое устройство не пришло за отведённое время");
    Ok(Exchange::Refused)
}

/// Ключ не передан: предложить отозвать подключившееся устройство (токен у него
/// уже есть). Ответ «да» — `DELETE /v1/devices/{id}`.
async fn offer_revoke(
    sess: &mut Session,
    name: &str,
    device: u32,
    ui: &mut impl Prompt,
) -> anyhow::Result<Exchange> {
    if device == 0 {
        return Ok(Exchange::Refused);
    }
    match ui
        .ask(&format!("Отозвать подключившееся устройство «{name}»?"))
        .await
    {
        Answer::Yes => match sess.command(UiCommand::RevokeDevice { id: device }).await? {
            UiResult::Ok => ui.say(&format!("устройство «{name}» отозвано")),
            UiResult::Error { message, .. } => ui.say(&format!("не отозвано: {message}")),
            other => anyhow::bail!("неожиданный ответ: {other:?}"),
        },
        Answer::No => {}
        Answer::Interrupted => return Ok(Exchange::Interrupted),
    }
    Ok(Exchange::Refused)
}

/// Отменить обмен на сервере: новое устройство узнает об этом и спросит пароль.
async fn cancel(sess: &mut Session, code: &str) {
    let _ = sess
        .command(UiCommand::CancelHandoff {
            code: code.to_owned(),
        })
        .await;
}

/// Новое устройство: получить ключ от источника (`commit` — из `Token.handoff`,
/// `secret` и `check` — `m=` и `h=` из ссылки QR). Цифры показываются всегда; без
/// `h` ключ принимается, только когда пользователь подтвердит их здесь. Ключ
/// сохраняется в `.selfsync/key`. Сбой сети — не повод бросать обмен.
pub async fn receive_key(
    sess: &mut Session,
    commit: Vec<u8>,
    secret: Option<String>,
    check: Option<String>,
    deadline_ms: i64,
    interval: Duration,
    ui: &mut impl Prompt,
) -> anyhow::Result<Exchange> {
    let mut r = sess
        .command(UiCommand::ReceiveKey {
            commit,
            link_secret: secret,
            check,
        })
        .await?;
    let mut said: Option<HandoffStep> = None;
    let mut confirmed: Option<String> = None;
    let mut wait = interval;
    loop {
        match r {
            UiResult::Handoff {
                step: HandoffStep::Done,
                ..
            } => {
                ui.say("ключ шифрования получен");
                return Ok(Exchange::Done);
            }
            UiResult::Handoff {
                step: HandoffStep::Confirm,
                sas: Some(sas),
                qr,
                offline,
                ..
            } => {
                if qr {
                    if said != Some(HandoffStep::Confirm) {
                        ui.say(&format!(
                            "цифры обмена: {}. При входе по QR на устройстве с кодом их сверять \
                             не нужно; если оно всё же показывает цифры и они не совпадают — \
                             нажмите там «нет» (не передавать)",
                            format_sas(&sas)
                        ));
                    }
                } else if confirmed.as_deref() != Some(sas.as_str()) {
                    let q = format!(
                        "цифры на этом устройстве: {}. Совпадают с экраном устройства, где открыт код?",
                        format_sas(&sas)
                    );
                    match ui.ask(&q).await {
                        Answer::Yes => {
                            confirmed = Some(sas);
                            ui.say("подтвердите передачу ключа на устройстве, где открыт код");
                            said = Some(HandoffStep::Confirm);
                            r = sess
                                .command(UiCommand::ReceivePoll {
                                    confirm: confirmed.clone(),
                                })
                                .await?;
                            continue;
                        }
                        a => {
                            // Отказ здесь доходит до источника: он увидит, что обмен прерван.
                            let _ = sess.command(UiCommand::CancelReceive).await;
                            if a == Answer::Interrupted {
                                return Ok(Exchange::Interrupted);
                            }
                            ui.say("цифры не совпали — возможна подмена, ключ не принят");
                            return Ok(Exchange::Refused);
                        }
                    }
                }
                said = Some(HandoffStep::Confirm);
                wait = next_pause(wait, interval, offline, ui);
            }
            UiResult::Handoff { offline, .. } => {
                if said.is_none() {
                    said = Some(HandoffStep::Waiting);
                    ui.say(
                        "ждём ключ шифрования с устройства, где открыт код \
                         (Ctrl-C — ввести пароль позже)…",
                    );
                }
                wait = next_pause(wait, interval, offline, ui);
            }
            UiResult::Error { message, .. } => {
                ui.say(&format!("ключ не пришёл: {message}"));
                return Ok(Exchange::Refused);
            }
            other => anyhow::bail!("неожиданный ответ: {other:?}"),
        }
        if now_ms() >= deadline_ms {
            let _ = sess.command(UiCommand::CancelReceive).await;
            return Ok(Exchange::Refused);
        }
        if !pause(wait).await {
            let _ = sess.command(UiCommand::CancelReceive).await;
            return Ok(Exchange::Interrupted);
        }
        r = sess
            .command(UiCommand::ReceivePoll {
                confirm: confirmed.clone(),
            })
            .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let l = connect_link(
            "https://notes.example.com:8443/s",
            "abcd1234efgh5678",
            "c2Vj-cmV0_",
            "szS-WgFatbGgO7YEQg1pWg",
        );
        assert_eq!(
            parse_link(&l),
            Some(Link {
                server: "https://notes.example.com:8443/s".into(),
                code: "abcd1234efgh5678".into(),
                secret: Some("c2Vj-cmV0_".into()),
                check: Some("szS-WgFatbGgO7YEQg1pWg".into()),
            })
        );
        assert_eq!(
            parse_link("https://notes.example.com/join/abcd1234efgh5678"),
            Some(Link {
                server: "https://notes.example.com".into(),
                code: "abcd1234efgh5678".into(),
                secret: None,
                check: None,
            })
        );
        // Как строит ссылку плагин (URLSearchParams).
        assert_eq!(
            parse_link(
                "obsidian://selfsync-connect?server=https%3A%2F%2Fa.example&code=x1&m=s&h=c"
            ),
            Some(Link {
                server: "https://a.example".into(),
                code: "x1".into(),
                secret: Some("s".into()),
                check: Some("c".into()),
            })
        );
        assert_eq!(parse_link("obsidian://selfsync-connect?code=x"), None);
        assert_eq!(parse_link("https://a.example/"), None);
        assert_eq!(parse_link("ftp://a/join/x"), None);
    }

    #[test]
    fn sas_and_qr() {
        assert_eq!(format_sas("042917"), "042 917");
        let q = render_qr("obsidian://selfsync-connect?server=x&code=y&m=z").unwrap();
        assert!(q.lines().count() > 10);
        assert!(
            q.lines()
                .all(|l| l.chars().count() == q.lines().next().unwrap().chars().count())
        );
    }
}
