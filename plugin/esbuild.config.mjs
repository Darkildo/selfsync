// Сборка плагина: src/main.ts → main.js (CommonJS, как ждёт Obsidian). WASM
// встраивается байтами прямо в main.js: Obsidian загружает только main.js,
// manifest.json и styles.css.
import { builtinModules } from "node:module";
import esbuild from "esbuild";

const production = process.argv[2] === "production";

const ctx = await esbuild.context({
  entryPoints: ["src/main.ts"],
  bundle: true,
  format: "cjs",
  target: "es2022",
  platform: "browser",
  outfile: "main.js",
  external: ["obsidian", "electron", ...builtinModules, ...builtinModules.map((m) => `node:${m}`)],
  loader: { ".wasm": "binary" },
  alias: { "selfsync-wasm-bytes": "./pkg/selfsync_wasm_bg.wasm" },
  sourcemap: production ? false : "inline",
  minify: production,
  treeShaking: true,
  logLevel: "info",
  // import.meta.url есть только в асинхронной загрузке обвязки wasm-bindgen, а мы
  // инициализируем WASM синхронно из встроенных байтов.
  logOverride: { "empty-import-meta": "silent" },
});

if (production) {
  await ctx.rebuild();
  await ctx.dispose();
} else {
  await ctx.watch();
}
