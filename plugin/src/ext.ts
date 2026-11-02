// Точка входа для надстроек над плагином (selfsync-cloud и подобных), которые
// включают ядро копией (`core/`) и строят свой плагин на его классах.
//
// СТАБИЛЬНЫЙ API: всё, что экспортировано здесь, меняется только с записью в
// CHANGELOG и по возможности с переходным периодом (старое имя рядом с новым).
// Импорт из других модулей `plugin/src` надстройке не гарантирован — внутреннее
// устройство меняется без предупреждения. CI selfsync собирает и тестирует
// selfsync-cloud против текущего ядра (джоб `extension`).

export { default as SelfsyncPlugin, type KeyReceiver } from "./main.ts";
export {
  DEFAULT_SETTINGS,
  normalizeServer,
  type Row,
  type Section,
  type SectionId,
  SelfsyncSettingTab,
  type Settings,
} from "./settings.ts";
export type {
  DiagCheck,
  EngineConfig,
  SyncStatus,
  UiCommand,
  UiResult,
} from "./types.ts";
export { initWasm, Runner } from "./engine.ts";
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
export { confirm } from "./ui/confirm.ts";
export { DiagnoseModal, hasProblems } from "./ui/diagnose.ts";
export { qrSvg } from "./ui/join.ts";
export { phraseError } from "./ui/recovery.ts";
export {
  argon2id,
  passwordStrength,
  phraseDecode,
  phraseEncode,
  protocolVersion,
  qrModules,
} from "../pkg/selfsync_wasm.js";
