//! `GET /join/{code}` — страница подключения нового устройства. Без внешних
//! ресурсов, открывается с телефона. Код при просмотре не расходуется.

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use http::{HeaderMap, StatusCode, header};

use super::public_base;
use crate::db::server;
use crate::state::AppState;

struct Texts {
    lang: &'static str,
    title: &'static str,
    intro: &'static str,
    step1: &'static str,
    step2: &'static str,
    step3: &'static str,
    button: &'static str,
    manual: &'static str,
    server: &'static str,
    code: &'static str,
    expires: &'static str,
    invalid_title: &'static str,
    invalid: &'static str,
}

const RU: Texts = Texts {
    lang: "ru",
    title: "Подключение устройства",
    intro: "Это устройство подключится к хранилищу заметок",
    step1: "Установите Obsidian (obsidian.md) и откройте хранилище, которое нужно синхронизировать (можно пустое).",
    step2: "В Obsidian: Настройки → Сторонние плагины → Обзор, найдите «Notesync», установите и включите.",
    step3: "Нажмите кнопку ниже — Obsidian откроется и подключится сам.",
    button: "Подключить",
    manual: "Если кнопка не сработала, введите в настройках плагина вручную:",
    server: "Сервер",
    code: "Код",
    expires: "Ссылка одноразовая и действует 15 минут.",
    invalid_title: "Ссылка недействительна",
    invalid: "Код подключения истёк или уже использован. Создайте новый в настройках плагина на подключённом устройстве.",
};

const EN: Texts = Texts {
    lang: "en",
    title: "Connect a device",
    intro: "This device will connect to the notes vault",
    step1: "Install Obsidian (obsidian.md) and open the vault you want to sync (an empty one is fine).",
    step2: "In Obsidian: Settings → Community plugins → Browse, find “Notesync”, install and enable it.",
    step3: "Tap the button below — Obsidian will open and connect by itself.",
    button: "Connect",
    manual: "If the button does nothing, enter this in the plugin settings:",
    server: "Server",
    code: "Code",
    expires: "The link works once and expires in 15 minutes.",
    invalid_title: "Link is not valid",
    invalid: "The connection code has expired or was already used. Create a new one in the plugin settings on a connected device.",
};

fn texts(headers: &HeaderMap) -> &'static Texts {
    let al = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if al.trim_start().to_ascii_lowercase().starts_with("ru") {
        &RU
    } else {
        &EN
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(char::from(b));
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

const STYLE: &str = "body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;max-width:34rem;margin:0 auto;padding:1.5rem;line-height:1.5;color:#1f2328;background:#fff}
h1{font-size:1.4rem}ol{padding-left:1.2rem}li{margin:.6rem 0}
a.btn{display:block;text-align:center;background:#7c3aed;color:#fff;text-decoration:none;padding:1rem;border-radius:.6rem;font-size:1.1rem;font-weight:600;margin:1.5rem 0}
code{background:#f3f4f6;padding:.15rem .35rem;border-radius:.3rem;word-break:break-all}
.muted{color:#6b7280;font-size:.9rem}
@media (prefers-color-scheme:dark){body{background:#111827;color:#e5e7eb}code{background:#1f2937}.muted{color:#9ca3af}}";

pub async fn page(
    State(st): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    let t = texts(&headers);
    let code = code.trim().to_ascii_lowercase();
    let valid = if code.len() == 16 && code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        let c2 = code.clone();
        st.server_db(move |c| Ok(server::peek_join_code(c, &c2)?))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let mut resp = match valid {
        None => {
            let html = format!(
                "<!doctype html><html lang=\"{}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{STYLE}</style></head><body><h1>{}</h1><p>{}</p></body></html>",
                t.lang, t.invalid_title, t.invalid_title, t.invalid
            );
            (StatusCode::NOT_FOUND, Html(html)).into_response()
        }
        Some((vault, name)) => {
            let base = public_base(&st, &headers);
            let link = format!(
                "obsidian://notesync-connect?server={}&code={}",
                pct(&base),
                pct(&code)
            );
            let html = format!(
                "<!doctype html><html lang=\"{lang}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{STYLE}</style></head><body>\
<h1>{title}</h1><p>{intro} <b>{vault}</b> ({name}).</p>\
<ol><li>{s1}</li><li>{s2}</li><li>{s3}</li></ol>\
<a class=\"btn\" href=\"{link}\">{button}</a>\
<p>{manual}</p><p>{server_l}: <code>{base}</code><br>{code_l}: <code>{code}</code></p>\
<p class=\"muted\">{expires}</p></body></html>",
                lang = t.lang,
                title = t.title,
                intro = t.intro,
                vault = esc(&vault),
                name = esc(&name),
                s1 = t.step1,
                s2 = t.step2,
                s3 = t.step3,
                link = esc(&link),
                button = t.button,
                manual = t.manual,
                server_l = t.server,
                base = esc(&base),
                code_l = t.code,
                code = esc(&code),
                expires = t.expires,
            );
            Html(html).into_response()
        }
    };
    let h = resp.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    h.insert(
        "content-security-policy",
        http::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    h.insert(
        "referrer-policy",
        http::HeaderValue::from_static("no-referrer"),
    );
    resp
}
