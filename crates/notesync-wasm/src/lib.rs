//! Обёртка ядра для плагина: `WasmEngine.handle(now, event)` → массив действий.
//!
//! События и действия — обычные JS-объекты в форме serde-типов ядра
//! (`{ type: "done", id, result: { type: "http", status, headers, body } }`), байты —
//! `Uint8Array`. Числа — `number`: все счётчики и размеры заведомо меньше 2^53.

use notesync_core::engine::{Engine, EngineConfig, Event};
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
    notesync_core::crypto::password_strength_bits(password)
}

/// Нормализованный путь vault'а (NFC, без лишних `/`) или `undefined`, если путь
/// не синхронизируется.
#[wasm_bindgen(js_name = normalizePath)]
pub fn normalize_path(path: &str) -> Option<String> {
    notesync_core::path::VaultPath::normalize(path)
        .ok()
        .map(|p| p.as_str().to_owned())
}

/// Версия протокола, которую говорит ядро.
#[wasm_bindgen(js_name = protocolVersion)]
pub fn protocol_version() -> u32 {
    notesync_core::proto::PROTO_VERSION
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
