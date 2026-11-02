// npm run i18n:check -- <код>…   проверить файлы src/i18n/<код>.ts (можно ещё не в реестре);
// npm run i18n:check             все файлы языков и покрытие по каждому.
// Код выхода 1, если есть ошибки.

import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { LOCALES } from "../src/i18n/locales.ts";
import { lintDict } from "../src/i18n/lint.ts";

const dir = join(dirname(fileURLToPath(import.meta.url)), "../src/i18n");
const NOT_LOCALES = new Set(["en", "index", "lint", "locales"]);
const all = readdirSync(dir)
  .filter((f) => f.endsWith(".ts"))
  .map((f) => f.slice(0, -3))
  .filter((c) => !NOT_LOCALES.has(c))
  .sort();
const codes = process.argv.slice(2).map((c) => c.toLowerCase());

let failed = false;
for (const code of codes.length > 0 ? codes : all) {
  let dict: Record<string, unknown>;
  try {
    const mod = (await import(pathToFileURL(join(dir, `${code}.ts`)).href)) as { default?: Record<string, unknown> };
    if (!mod.default) throw new Error("нет export default");
    dict = mod.default;
  } catch (e) {
    console.log(`${code}: не загружается — ${e instanceof Error ? e.message : String(e)}`);
    failed = true;
    continue;
  }
  const r = lintDict(dict);
  const pct = Math.round((100 * r.translated) / r.total);
  const reg = Object.hasOwn(LOCALES, code) ? "" : " (нет в locales.ts)";
  console.log(`${code}: ${r.translated}/${r.total} (${pct}%), ошибок ${r.errors.length}, предупреждений ${r.warnings.length}${reg}`);
  if (codes.length > 0) for (const w of r.warnings) console.log(`  предупреждение: ${w}`);
  for (const e of r.errors) console.log(`  ошибка: ${e}`);
  if (r.errors.length > 0) failed = true;
}
process.exitCode = failed ? 1 : 0;
