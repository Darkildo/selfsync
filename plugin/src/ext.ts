// Точка входа для надстроек над плагином (selfsync-cloud и подобных), которые
// включают ядро копией (`core/`) и строят свой плагин на его классах.
//
// СТАБИЛЬНЫЙ API: всё, что экспортировано здесь, меняется только с записью в
// CHANGELOG и по возможности с переходным периодом (старое имя рядом с новым).
// Импорт из других модулей `plugin/src` надстройке не гарантирован — внутреннее
// устройство меняется без предупреждения. CI selfsync собирает и тестирует
// selfsync-cloud против текущего ядра (джоб `extension`).
//
// Модули без Obsidian (WASM, строки, lintDict) есть и отдельно — в ext-pure.ts:
// их можно импортировать в Node, где пакета `obsidian` нет.

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
export { Runner } from "./engine.ts";
// Без Obsidian (WASM, строки, проверка словарей) — ext-pure.ts, здесь реэкспорт.
export * from "./ext-pure.ts";
export { confirm } from "./ui/confirm.ts";
export { DiagnoseModal, hasProblems } from "./ui/diagnose.ts";
export { qrSvg } from "./ui/join.ts";
export { phraseError } from "./ui/recovery.ts";
