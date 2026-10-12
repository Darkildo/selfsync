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
    pub fn new(config: JsValue, index: Option<Vec<u8>>) -> Result<WasmEngine, JsError> {
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
    pub fn handle(&mut self, now: f64, event: JsValue) -> Result<JsValue, JsError> {
        let event: Event = from_js(event, "event")?;
        // Миллисекунды Unix-времени: целые и далеко от пределов i64.
        let actions = self.inner.handle(now as i64, event);
        to_js(&actions)
    }

    /// Текущий статус (то же, что последний `Action::Status`).
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
