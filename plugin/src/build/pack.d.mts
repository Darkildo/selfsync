// Типы плагина сборки (pack.mjs) для тестов и надстроек на TypeScript.
import type { Plugin } from "esbuild";

export interface PackOptions {
  /** Путь к selfsync_wasm_bg.wasm: импорт `selfsync-wasm-bytes` даёт его сжатым. */
  wasm?: string;
  /** Реестры переводов (locales.ts) и языки, которые остаются несжатыми. */
  locales?: { registry: string; keep?: string[] }[];
}

export function pack(options: PackOptions): Plugin;
