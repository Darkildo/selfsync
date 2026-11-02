// Сборка со сжатыми WASM и переводами (src/build/pack.mjs): main.js меньше, а
// ведёт себя так же — WASM запускается, переводы те же, распаковывается только
// выбранный язык.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import esbuild from "esbuild";

import { pack } from "../src/build/pack.mjs";
import { LOCALES as SOURCE } from "../src/i18n/locales.ts";

const DIR = fileURLToPath(new URL("..", import.meta.url));
const WASM = `${DIR}pkg/selfsync_wasm_bg.wasm`;

interface Bundle {
  bytes: Uint8Array;
  LOCALES: Record<string, Record<string, string>>;
  initWasm(b: Uint8Array): void;
  setLanguage(code: string): void;
  t(key: string): string;
  protocolVersion(): number;
}

async function bundle(): Promise<{ mod: Bundle; size: number }> {
  const out = await esbuild.build({
    stdin: {
      contents: `
        export { default as bytes } from "selfsync-wasm-bytes";
        export { initWasm } from "./src/engine.ts";
        export { setLanguage, t } from "./src/i18n.ts";
        export { LOCALES } from "./src/i18n/locales.ts";
        export { protocolVersion } from "./pkg/selfsync_wasm.js";`,
      resolveDir: DIR,
      loader: "ts",
    },
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "es2022",
    write: false,
    logLevel: "error",
    logOverride: { "empty-import-meta": "silent" },
    plugins: [pack({ wasm: WASM, locales: [{ registry: `${DIR}src/i18n/locales.ts`, keep: ["ru"] }] })],
  });
  const code = out.outputFiles[0]!.text;
  const mod = (await import(`data:text/javascript;base64,${Buffer.from(code).toString("base64")}`)) as Bundle;
  return { mod, size: Buffer.byteLength(code) };
}

test("сборка: WASM сжат и запускается, распакован только выбранный язык", async () => {
  const { mod, size } = await bundle();
  const raw = readFileSync(WASM);
  assert.notDeepEqual([...mod.bytes.subarray(0, 4)], [0, 0x61, 0x73, 0x6d], "в сборке WASM сжат");
  assert.ok(mod.bytes.length < raw.length * 0.6, `сжатый WASM ${mod.bytes.length} из ${raw.length}`);
  assert.ok(size < raw.length * 1.6, `сборка ${size} байт`);

  mod.initWasm(mod.bytes);
  assert.equal(mod.protocolVersion(), 1);

  const codes = Object.keys(SOURCE).sort();
  assert.deepEqual(Object.keys(mod.LOCALES).sort(), codes, "те же языки, что в реестре");
  const unpacked = () => codes.filter((c) => "value" in Object.getOwnPropertyDescriptor(mod.LOCALES, c)!);
  assert.deepEqual(unpacked(), ["ru"], "до выбора языка распакован только ru (он не сжимается)");

  mod.setLanguage("de");
  assert.equal(mod.t("status.idle"), SOURCE["de"]!["status.idle"]);
  assert.deepEqual(unpacked(), ["de", "ru"], "после выбора — ещё только de");
});

test("сборка: каждый сжатый словарь совпадает с исходным", async () => {
  const { mod } = await bundle();
  for (const [code, dict] of Object.entries(SOURCE)) {
    assert.deepEqual({ ...mod.LOCALES[code] }, { ...dict }, code);
  }
});

test("ext-pure.ts загружается без пакета obsidian", async () => {
  const pure = await import("../src/ext-pure.ts");
  assert.equal(typeof pure.initWasm, "function");
  assert.equal(typeof pure.lintDict, "function");
  assert.equal(typeof pure.inflateRaw, "function");
});
