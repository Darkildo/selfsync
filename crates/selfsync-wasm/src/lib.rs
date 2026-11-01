//! Обёртка ядра для плагина: `WasmEngine.handle(now, event)` → массив действий.
//!
//! События и действия — обычные JS-объекты в форме serde-типов ядра
//! (`{ type: "done", id, result: { type: "http", status, headers, body } }`), байты —
//! `Uint8Array`. Числа — `number`: все счётчики и размеры заведомо меньше 2^53.

use selfsync_core::engine::{Engine, EngineConfig, Event};
use serde::Serialize;
use wasm_bindgen::prelude::*;

fn serializer() -> serde_wasm_bindgen::Serializer {
    serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true)
}

fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsError> {
    v.serialize(&serializer())
        .map_err(|e| JsError::new(&e.to_string()))
}

fn from_js<T: serde::de::DeserializeOwned>(v: JsValue, what: &str) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(v).map_err(|e| JsError::new(&format!("{what}: {e}")))
}

#[wasm_bindgen]
pub struct WasmEngine {
    inner: Engine,
}

#[wasm_bindgen]
impl WasmEngine {
    /// `config` — объект настроек (`EngineConfig`, поля в camelCase, все необязательны);
    /// `index` — сохранённый снимок индекса или `undefined`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "unknown")] config: JsValue,
        index: Option<Vec<u8>>,
    ) -> Result<WasmEngine, JsError> {
        console_error_panic_hook::set_once();
        let config: EngineConfig = if config.is_undefined() || config.is_null() {
            EngineConfig::default()
        } else {
            from_js(config, "config")?
        };
        Ok(WasmEngine {
            inner: Engine::new(config, index.as_deref()),
        })
    }

    /// Обрабатывает событие; возвращает массив действий для исполнителя.
    // В .d.ts — unknown, а не any: типы событий и действий — в plugin/src/types.ts.
    #[wasm_bindgen(unchecked_return_type = "unknown")]
    pub fn handle(
        &mut self,
        now: f64,
        #[wasm_bindgen(unchecked_param_type = "unknown")] event: JsValue,
    ) -> Result<JsValue, JsError> {
        let event: Event = from_js(event, "event")?;
        // Миллисекунды Unix-времени: целые и далеко от пределов i64.
        let actions = self.inner.handle(now as i64, event);
        to_js(&actions)
    }

    /// Текущий статус (то же, что последний `Action::Status`).
    #[wasm_bindgen(unchecked_return_type = "unknown")]
    pub fn status(&self) -> Result<JsValue, JsError> {
        to_js(&self.inner.status())
    }

    /// Есть ли неотправленные локальные изменения (перед выгрузкой плагина).
    #[wasm_bindgen(js_name = hasPending)]
    pub fn has_pending(&self) -> bool {
        self.inner.has_pending()
    }

    /// Когда движок хочет следующий `tick` (мс) или `undefined`.
    #[wasm_bindgen(js_name = nextWake)]
    pub fn next_wake(&self) -> Option<f64> {
        self.inner.next_wake().map(|t| t as f64)
    }
}

/// Оценка стойкости пароля в битах (подсказка в интерфейсе, не запрет).
#[wasm_bindgen(js_name = passwordStrength)]
pub fn password_strength(password: &str) -> u32 {
    selfsync_core::crypto::password_strength_bits(password)
}

/// Нормализованный путь vault'а (NFC, без лишних `/`) или `undefined`, если путь
/// не синхронизируется.
#[wasm_bindgen(js_name = normalizePath)]
pub fn normalize_path(path: &str) -> Option<String> {
    selfsync_core::path::VaultPath::normalize(path)
        .ok()
        .map(|p| p.as_str().to_owned())
}

/// Версия протокола, которую говорит ядро.
#[wasm_bindgen(js_name = protocolVersion)]
pub fn protocol_version() -> u32 {
    selfsync_core::proto::PROTO_VERSION
}

/// Минорная версия протокола, с которой сервер умеет устройства только для чтения
/// (сравнивается с `SyncStatus.serverMinor`).
#[wasm_bindgen(js_name = readOnlyMinor)]
pub fn read_only_minor() -> u32 {
    selfsync_core::proto::MINOR_READ_ONLY
}

/// Клиент устройства (`plugin/0.3.0`) старее `min`; пустой или неразборчивый — тоже.
/// Минорная версия протокола, с которой сервер стирает старые версии файла.
#[wasm_bindgen(js_name = quotaMinor)]
pub fn quota_minor() -> u32 {
    selfsync_core::proto::MINOR_QUOTA
}

#[wasm_bindgen(js_name = clientOlderThan)]
pub fn client_older_than(client: &str, min: &str) -> bool {
    selfsync_core::version::older_than(client, min)
}

/// QR-код строки (ссылки подключения): `[ширина, модули…]`, модуль 1 — тёмный.
/// Рисует интерфейс: так в WASM не попадает рендерер SVG/картинок.
#[wasm_bindgen(js_name = qrModules)]
pub fn qr_modules(text: &str) -> Result<Vec<u8>, JsError> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| JsError::new(&e.to_string()))?;
    let width = u8::try_from(code.width()).map_err(|_| JsError::new("QR-код слишком большой"))?;
    let mut out = Vec::with_capacity(1 + code.width() * code.width());
    out.push(width);
    out.extend(
        code.to_colors()
            .into_iter()
            .map(|c| u8::from(c == qrcode::Color::Dark)),
    );
    Ok(out)
}

/// Argon2id(пароль, соль) → 32 байта; параметры — в тех же пределах, что у записи
/// ключа vault'а. Для надстроек (вход в аккаунт сервиса), ядру синка не нужен.
#[wasm_bindgen(js_name = argon2id)]
pub fn argon2id(
    password: &str,
    salt: &[u8],
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Vec<u8>, JsError> {
    let p = selfsync_core::crypto::KdfParams {
        m_cost_kib,
        t_cost,
        p_cost,
    };
    selfsync_core::crypto::argon2id(password, salt, p)
        .map(|k| k.to_vec())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// 32 байта энтропии → 24 слова BIP39 (как фраза восстановления vault'а).
#[wasm_bindgen(js_name = phraseEncode)]
pub fn phrase_encode(entropy: &[u8]) -> Result<Vec<String>, JsError> {
    let e: &[u8; 32] = entropy
        .try_into()
        .map_err(|_| JsError::new("нужно ровно 32 байта"))?;
    Ok(selfsync_core::bip39::encode(e))
}

/// Фраза → 32 байта энтропии; неверная фраза — `undefined` (причину даёт `checkPhrase`).
#[wasm_bindgen(js_name = phraseDecode)]
pub fn phrase_decode(phrase: &str) -> Option<Vec<u8>> {
    selfsync_core::bip39::decode(phrase)
        .ok()
        .map(|e| e.to_vec())
}

/// Проверка фразы восстановления до отправки: `undefined` — в порядке, иначе
/// `count:N` (слов не 24), `word:I` (слово I, с единицы, не из словаря) или
/// `checksum`. Текст сообщения формирует интерфейс.
#[wasm_bindgen(js_name = checkPhrase)]
pub fn check_phrase(phrase: &str) -> Option<String> {
    use selfsync_core::bip39::PhraseError;
    match selfsync_core::bip39::decode(phrase) {
        Ok(_) => None,
        Err(PhraseError::WordCount(n)) => Some(format!("count:{n}")),
        Err(PhraseError::UnknownWord { index, .. }) => Some(format!("word:{index}")),
        Err(PhraseError::Checksum) => Some("checksum".to_owned()),
    }
}

/// Минорная версия протокола, с которой сервер умеет передачу ключа по коду.
#[wasm_bindgen(js_name = handoffMinor)]
pub fn handoff_minor() -> u32 {
    selfsync_core::proto::MINOR_HANDOFF
}

// Примитивы передачи ключа по коду (`selfsync_core::handoff`) для надстроек:
// у облака свой обмен (`/account/handoff`) со своей меткой домена (`prefix`),
// криптография — та же. Секрет пары — 32 байта скаляра; хранит его вызывающий.

fn js_err(e: selfsync_core::crypto::CryptoError) -> JsError {
    JsError::new(&e.to_string())
}

fn ephemeral(secret: &[u8]) -> Result<selfsync_core::handoff::Ephemeral, JsError> {
    let b: &[u8; 32] = secret
        .try_into()
        .map_err(|_| JsError::new("секрет пары — 32 байта"))?;
    selfsync_core::handoff::Ephemeral::from_secret(b).map_err(js_err)
}

/// Новая пара ключей обмена: секрет (32 байта).
#[wasm_bindgen(js_name = handoffGenerate)]
pub fn handoff_generate() -> Result<Vec<u8>, JsError> {
    let e = selfsync_core::handoff::Ephemeral::generate().map_err(js_err)?;
    Ok(e.secret_bytes().to_vec())
}

/// Открытый ключ пары (несжатая точка, 65 байт).
#[wasm_bindgen(js_name = handoffPublic)]
pub fn handoff_public(secret: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(ephemeral(secret)?.public().to_vec())
}

#[wasm_bindgen(js_name = handoffValidPublic)]
pub fn handoff_valid_public(public: &[u8]) -> bool {
    selfsync_core::handoff::valid_public(public)
}

/// `sha256(A)`.
#[wasm_bindgen(js_name = handoffCommit)]
pub fn handoff_commit(public: &[u8]) -> Vec<u8> {
    selfsync_core::handoff::commit(public).to_vec()
}

/// `h` для QR: первые 16 байт обязательства.
#[wasm_bindgen(js_name = handoffCheck)]
pub fn handoff_check(commit: &[u8]) -> Vec<u8> {
    selfsync_core::handoff::commit_check(commit).to_vec()
}

/// Обязательство от сервера совпадает с `h` из QR (за постоянное время).
#[wasm_bindgen(js_name = handoffCheckMatches)]
pub fn handoff_check_matches(check: &[u8], commit: &[u8]) -> bool {
    selfsync_core::handoff::check_commit_check(check, commit)
}

/// `HMAC-SHA256(секрет из QR, B)`.
#[wasm_bindgen(js_name = handoffMac)]
pub fn handoff_mac(link_secret: &[u8], public: &[u8]) -> Vec<u8> {
    selfsync_core::handoff::link_mac(link_secret, public).to_vec()
}

/// Шесть цифр для сверки (`a` — ключ источника, `b` — нового устройства).
#[wasm_bindgen(js_name = handoffSas)]
pub fn handoff_sas(prefix: &str, a: &[u8], b: &[u8]) -> String {
    selfsync_core::handoff::sas_digits(&selfsync_core::handoff::Labels::new(prefix), a, b)
}

/// Источник: обёртка `payload` для нового устройства с ключом `joiner`.
#[wasm_bindgen(js_name = handoffWrap)]
pub fn handoff_wrap(
    prefix: &str,
    source_secret: &[u8],
    joiner: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>, JsError> {
    let l = selfsync_core::handoff::Labels::new(prefix);
    selfsync_core::handoff::wrap(&l, &ephemeral(source_secret)?, joiner, payload).map_err(js_err)
}

/// Новое устройство: открыть обёртку ключом источника `source`.
#[wasm_bindgen(js_name = handoffUnwrap)]
pub fn handoff_unwrap(
    prefix: &str,
    joiner_secret: &[u8],
    source: &[u8],
    wrapped: &[u8],
) -> Result<Vec<u8>, JsError> {
    let l = selfsync_core::handoff::Labels::new(prefix);
    selfsync_core::handoff::unwrap(&l, &ephemeral(joiner_secret)?, source, wrapped)
        .map(|k| k.to_vec())
        .map_err(js_err)
}
