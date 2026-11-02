// Плагин esbuild: кладёт в main.js сжатыми WASM ядра и переводы. Работает при
// сборке (Node), в Obsidian не попадает. Надстройки (selfsync-cloud) получают его
// вместе с ядром (core/plugin/src/build/pack.mjs) и подключают к своей сборке.
//
// WASM. Импорт `selfsync-wasm-bytes` даёт байты модуля, сжатые deflate;
// initWasm (engine.ts) распаковывает их сам. Псевдоним `selfsync-wasm-bytes`
// в настройках esbuild при этом не нужен: он перехватывает импорт раньше плагина.
//
// Переводы. Реестр (locales.ts: `export const LOCALES = { … }`) подменяется
// модулем с тем же экспортом, где каждый язык, кроме `keep`, лежит сжатым и
// распаковывается при первом обращении к LOCALES[код]. Плагин выбирает один
// язык, остальные в память не попадают. Словарь хранится значениями в порядке
// общего списка ключей: ключи не повторяются в каждом языке.

import { readFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";
import { deflateRawSync } from "node:zlib";

const INFLATE = fileURLToPath(new URL("../inflate.ts", import.meta.url));
const WASM_NS = "selfsync-pack-wasm";
const BLOB_NS = "selfsync-pack-blob";

function deflate(bytes) {
  return deflateRawSync(bytes, { level: 9, memLevel: 9 });
}

/** Словари реестра — собранные и выполненные здесь же, в Node. */
async function readRegistry(esbuild, registry) {
  const out = await esbuild.build({
    entryPoints: [registry],
    bundle: true,
    format: "cjs",
    platform: "neutral",
    write: false,
    logLevel: "silent",
  });
  // Словари — данные без зависимостей: модуль выполняется в отдельном контексте.
  const module = { exports: {} };
  runInNewContext(out.outputFiles[0].text, { module, exports: module.exports });
  const mod = module.exports;
  const names = Object.keys(mod);
  if (names.length !== 1 || names[0] !== "LOCALES") {
    throw new Error(`pack: ${registry} должен экспортировать только LOCALES, а экспортирует ${names.join(", ")}`);
  }
  return mod.LOCALES;
}

/** Сжатые словари одним блоком и модуль, который их отдаёт. */
function packLocales(registry, locales, keep) {
  const keys = [];
  const seen = new Set();
  for (const dict of Object.values(locales)) {
    for (const k of Object.keys(dict)) {
      if (!seen.has(k)) {
        seen.add(k);
        keys.push(k);
      }
    }
  }
  const parts = [];
  const table = {};
  let offset = 0;
  for (const [code, dict] of Object.entries(locales)) {
    if (keep.includes(code)) continue;
    const raw = Buffer.from(JSON.stringify(keys.map((k) => dict[k] ?? null)));
    const packed = deflate(raw);
    table[code] = [offset, packed.length, raw.length];
    parts.push(packed);
    offset += packed.length;
  }
  const dir = dirname(registry);
  const imports = keep.map((code, i) => {
    const file = join(dir, `${code}.ts`);
    if (!existsSync(file)) throw new Error(`pack: нет ${file} (keep: ${code})`);
    return `import keep${i} from ${JSON.stringify(file)};`;
  });
  const kept = keep.map((code, i) => `${JSON.stringify(code)}: keep${i}`).join(", ");
  const source = `${imports.join("\n")}
import { inflateRaw } from ${JSON.stringify(INFLATE)};
import blob from ${JSON.stringify(registry)};
const KEYS = ${JSON.stringify(keys)};
const PACKED = ${JSON.stringify(table)};
export const LOCALES = { ${kept} };
for (const code of Object.keys(PACKED)) {
  Object.defineProperty(LOCALES, code, {
    enumerable: true,
    configurable: true,
    get() {
      const [off, len, raw] = PACKED[code];
      const values = JSON.parse(new TextDecoder().decode(inflateRaw(blob.subarray(off, off + len), raw)));
      const dict = {};
      values.forEach((v, i) => {
        if (v !== null) dict[KEYS[i]] = v;
      });
      Object.defineProperty(LOCALES, code, { value: dict, enumerable: true, configurable: true, writable: true });
      return dict;
    },
  });
}
`;
  return { source, blob: Buffer.concat(parts) };
}

/**
 * @param {{ wasm?: string, locales?: { registry: string, keep?: string[] }[] }} options
 *   wasm — путь к selfsync_wasm_bg.wasm; locales — реестры переводов (абсолютные пути
 *   или от текущего каталога) и языки, которые остаются несжатыми.
 */
export function pack(options) {
  const wasm = options.wasm && resolve(options.wasm);
  const registries = new Map((options.locales ?? []).map((l) => [resolve(l.registry), l.keep ?? []]));
  const blobs = new Map();
  return {
    name: "selfsync-pack",
    setup(build) {
      if (wasm) {
        build.onResolve({ filter: /^selfsync-wasm-bytes$/ }, () => ({ path: wasm, namespace: WASM_NS }));
        build.onLoad({ filter: /.*/, namespace: WASM_NS }, (args) => ({
          contents: deflate(readFileSync(args.path)),
          loader: "binary",
        }));
      }
      if (registries.size === 0) return;
      // Блок словарей — отдельный модуль с двоичным содержимым (импорт по пути реестра).
      build.onResolve({ filter: /.*/ }, (args) => {
        if (args.namespace === "file" && args.importer && registries.has(args.importer) && registries.has(args.path)) {
          return { path: args.path, namespace: BLOB_NS };
        }
        return undefined;
      });
      build.onLoad({ filter: /.*/, namespace: BLOB_NS }, (args) => ({ contents: blobs.get(args.path), loader: "binary" }));
      build.onLoad({ filter: /locales\.ts$/ }, async (args) => {
        const keep = registries.get(args.path);
        if (!keep) return undefined;
        const { source, blob } = packLocales(args.path, await readRegistry(build.esbuild, args.path), keep);
        blobs.set(args.path, blob);
        return { contents: source, loader: "js", resolveDir: dirname(args.path), watchFiles: [args.path] };
      });
    },
  };
}
