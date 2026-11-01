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

/// Ввод-вывод обмена: терминал или сценарий теста.
pub trait Prompt {
    fn say(&mut self, text: &str);
    /// Вопрос «да/нет»; по умолчанию — нет.
    fn ask(&mut self, question: &str) -> bool;
}

/// Терминал: сообщения — в stderr, ответ — строка из stdin.
pub struct Terminal;

impl Prompt for Terminal {
    fn say(&mut self, text: &str) {
        eprintln!("{text}");
    }

    fn ask(&mut self, question: &str) -> bool {
        use std::io::{BufRead, Write};
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(
            line.trim().to_lowercase().as_str(),
            "y" | "yes" | "д" | "да"
        )
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
}

/// `obsidian://selfsync-connect?server=…&code=…[&m=…]` (QR) или страница сервера
/// `https://сервер/join/<код>`.
pub fn parse_link(s: &str) -> Option<Link> {
    let s = s.trim();
    if let Some(q) = s.strip_prefix("obsidian://selfsync-connect?") {
        let mut server = None;
        let mut code = None;
        let mut secret = None;
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=')?;
            let v = percent_decode(v)?;
            match k {
                "server" => server = Some(v),
                "code" => code = Some(v),
                "m" => secret = Some(v),
                _ => {}
            }
        }
        return Some(Link {
            server: server?.trim_end_matches('/').to_owned(),
            code: code?,
            secret,
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

/// Ссылка для QR с секретом обмена — та же, что строит плагин. Секрет не проходит
/// через сервер: QR ведёт сразу в Obsidian.
pub fn connect_link(server: &str, code: &str, secret: &str) -> String {
    format!(
        "obsidian://selfsync-connect?server={}&code={}&m={}",
        percent_encode(server),
        percent_encode(code),
        percent_encode(secret)
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

/// Источник: ждать новое устройство по коду и передать ему ключ с согласия
/// пользователя (при коде, введённом руками, — после сверки цифр). `true` —
/// ключ передан.
pub async fn give_key(
    sess: &mut Session,
    code: &str,
    deadline_ms: i64,
    interval: Duration,
    ui: &mut dyn Prompt,
) -> anyhow::Result<bool> {
    let mut confirm: Option<String> = None;
    let mut waiting_said = false;
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
                return Ok(true);
            }
            UiResult::Handoff {
                step: HandoffStep::Waiting,
                ..
            } => {
                if !waiting_said {
                    ui.say("ждём новое устройство (Ctrl-C — выйти)…");
                    waiting_said = true;
                }
            }
            UiResult::Handoff {
                step: HandoffStep::Confirm,
                name,
                sas,
                qr,
            } => {
                let sas = sas.unwrap_or_default();
                let question = if qr {
                    format!("«{name}» подключилось по QR-коду. Передать ему ключ шифрования?")
                } else {
                    format!(
                        "«{name}» подключилось по коду. На нём должны быть цифры {}. \
                         Передать ключ шифрования, только если совпадают?",
                        format_sas(&sas)
                    )
                };
                if !ui.ask(&question) {
                    cancel(sess, code).await;
                    ui.say("ключ не передан: на новом устройстве понадобится пароль");
                    return Ok(false);
                }
                confirm = Some(sas);
                // Ответ — сразу, без паузы.
                continue;
            }
            UiResult::Error { message, .. } => {
                ui.say(&format!(
                    "ключ не передан: {message}. На новом устройстве можно ввести пароль"
                ));
                return Ok(false);
            }
            other => anyhow::bail!("неожиданный ответ: {other:?}"),
        }
        if !pause(interval).await {
            cancel(sess, code).await;
            return Ok(false);
        }
    }
    cancel(sess, code).await;
    ui.say("новое устройство не пришло за отведённое время");
    Ok(false)
}

async fn cancel(sess: &mut Session, code: &str) {
    let _ = sess
        .command(UiCommand::CancelHandoff {
            code: code.to_owned(),
        })
        .await;
}

/// Новое устройство: получить ключ от источника (`commit` — из `Token.handoff`,
/// `secret` — `m=` из ссылки). Ключ запоминается в `.selfsync/key`. `true` —
/// получен.
pub async fn receive_key(
    sess: &mut Session,
    commit: Vec<u8>,
    secret: Option<String>,
    deadline_ms: i64,
    interval: Duration,
    ui: &mut dyn Prompt,
) -> anyhow::Result<bool> {
    let by_qr = secret.is_some();
    let mut r = sess
        .command(UiCommand::ReceiveKey {
            commit,
            link_secret: secret,
        })
        .await?;
    let mut said: Option<HandoffStep> = None;
    loop {
        match r {
            UiResult::Handoff {
                step: HandoffStep::Done,
                ..
            } => {
                ui.say("ключ шифрования получен");
                return Ok(true);
            }
            UiResult::Handoff { step, sas, .. } => {
                if said != Some(step) {
                    said = Some(step);
                    match (step, sas) {
                        (HandoffStep::Confirm, Some(sas)) if !by_qr => ui.say(&format!(
                            "сверьте цифры с экраном устройства, где открыт код, и подтвердите там: {}",
                            format_sas(&sas)
                        )),
                        (HandoffStep::Confirm, _) => {
                            ui.say("подтвердите передачу ключа на устройстве, где открыт код");
                        }
                        _ => ui.say(
                            "ждём ключ шифрования с устройства, где открыт код \
                             (Ctrl-C — ввести пароль позже)…",
                        ),
                    }
                }
            }
            UiResult::Error { message, .. } => {
                ui.say(&format!("ключ не пришёл: {message}"));
                return Ok(false);
            }
            other => anyhow::bail!("неожиданный ответ: {other:?}"),
        }
        if now_ms() >= deadline_ms || !pause(interval).await {
            return Ok(false);
        }
        r = sess.command(UiCommand::ReceivePoll).await?;
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
        );
        assert_eq!(
            parse_link(&l),
            Some(Link {
                server: "https://notes.example.com:8443/s".into(),
                code: "abcd1234efgh5678".into(),
                secret: Some("c2Vj-cmV0_".into()),
            })
        );
        assert_eq!(
            parse_link("https://notes.example.com/join/abcd1234efgh5678"),
            Some(Link {
                server: "https://notes.example.com".into(),
                code: "abcd1234efgh5678".into(),
                secret: None,
            })
        );
        // Как строит ссылку плагин (URLSearchParams).
        assert_eq!(
            parse_link("obsidian://selfsync-connect?server=https%3A%2F%2Fa.example&code=x1&m=s"),
            Some(Link {
                server: "https://a.example".into(),
                code: "x1".into(),
                secret: Some("s".into()),
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
