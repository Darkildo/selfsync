// Точка входа для надстроек без Obsidian: то, что работает и в Node (тесты,
// сборка, проверка словарей), — WASM ядра, строки интерфейса, проверка переводов.
// ext.ts реэкспортирует всё отсюда; здесь же — модули, которые не тянут пакет
// `obsidian` (у него нет рантайма вне приложения).
//
// СТАБИЛЬНЫЙ API — на тех же условиях, что и ext.ts.

export { initWasm } from "./engine.ts";
export {
  formatBytes,
  formatDate,
  formatDateTime,
  formatTime,
  language,
  resolveLanguage,
  setLanguage,
  t,
} from "./i18n.ts";
export { type LintResult, lintDict } from "./i18n/lint.ts";
export { inflateRaw } from "./inflate.ts";
export {
  argon2id,
  passwordStrength,
  phraseDecode,
  phraseEncode,
  protocolVersion,
  qrModules,
} from "../pkg/selfsync_wasm.js";
