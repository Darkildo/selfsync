// Сборка плагина: src/main.ts → main.js (CommonJS, как ждёт Obsidian). WASM и
// переводы встраиваются прямо в main.js, сжатыми (src/build/pack.mjs): Obsidian
// загружает только main.js, manifest.json и styles.css.
import { builtinModules } from "node:module";
import esbuild from "esbuild";
import { pack } from "./src/build/pack.mjs";

const production = process.argv[2] === "production";

const ctx = await esbuild.context({
  entryPoints: ["src/main.ts"],
  bundle: true,
  format: "cjs",
  target: "es2022",
  platform: "browser",
  outfile: "main.js",
  external: ["obsidian", "electron", ...builtinModules, ...builtinModules.map((m) => `node:${m}`)],
  // en — эталон, ru — второй основной язык: они не сжимаются.
  plugins: [pack({ wasm: "pkg/selfsync_wasm_bg.wasm", locales: [{ registry: "src/i18n/locales.ts", keep: ["ru"] }] })],
  sourcemap: production ? false : "inline",
  minify: production,
  // Переводы на 70+ языков: без utf8 esbuild пишет не-ASCII как \uXXXX, и main.js
  // вырастает примерно в полтора раза.
  charset: "utf8",
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
