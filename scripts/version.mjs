#!/usr/bin/env node
// Версия проекта живёт в четырёх местах: Cargo.toml (workspace), manifest.json,
// versions.json и plugin/package.json. Каталог плагинов Obsidian берёт её из
// manifest.json в корне и ищет релиз с тегом ровно `x.y.z`.
//
//   node scripts/version.mjs check [ТЕГ]   все версии совпадают (и с тегом, если задан)
//   node scripts/version.mjs set X.Y.Z     выставить версию везде
//
// Для новой версии с другим минимальным Obsidian сначала поправьте minAppVersion
// в manifest.json: set запишет пару «версия → minAppVersion» в versions.json.

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const ROOT = join(import.meta.dirname, "..");
const SEMVER = /^\d+\.\d+\.\d+$/;
const CARGO_VERSION = /(\[workspace\.package\][^[]*?\nversion = ")([^"]+)(")/;

const file = (p) => join(ROOT, p);
const readJson = (p) => JSON.parse(readFileSync(file(p), "utf8"));
const writeJson = (p, v) => writeFileSync(file(p), JSON.stringify(v, null, 2) + "\n");

function versions() {
  const cargo = readFileSync(file("Cargo.toml"), "utf8").match(CARGO_VERSION)?.[2];
  return {
    "Cargo.toml": cargo,
    "manifest.json": readJson("manifest.json").version,
    "plugin/package.json": readJson("plugin/package.json").version,
  };
}

function check(tag) {
  const v = versions();
  const manifest = readJson("manifest.json");
  const errors = [];
  const want = v["manifest.json"];
  if (!SEMVER.test(want)) errors.push(`manifest.json: версия ${want} не в формате x.y.z`);
  for (const [where, got] of Object.entries(v)) {
    if (got !== want) errors.push(`${where}: ${got}, а в manifest.json ${want}`);
  }
  const compat = readJson("versions.json")[want];
  if (compat !== manifest.minAppVersion) {
    errors.push(`versions.json: для ${want} записано ${compat}, а minAppVersion в manifest.json ${manifest.minAppVersion}`);
  }
  if (tag !== undefined && tag !== want) errors.push(`тег ${tag} не совпадает с версией ${want} (нужен ровно x.y.z, без v)`);
  if (errors.length) {
    for (const e of errors) console.error(e);
    process.exit(1);
  }
  console.log(`версия ${want}${tag ? `, тег совпадает` : ""}`);
}

function set(next) {
  if (!SEMVER.test(next)) throw new Error(`версия ${next} не в формате x.y.z`);
  const cargo = readFileSync(file("Cargo.toml"), "utf8");
  if (!CARGO_VERSION.test(cargo)) throw new Error("в Cargo.toml не найдена версия workspace");
  writeFileSync(file("Cargo.toml"), cargo.replace(CARGO_VERSION, `$1${next}$3`));
  const manifest = readJson("manifest.json");
  writeJson("manifest.json", { ...manifest, version: next });
  writeJson("versions.json", { ...readJson("versions.json"), [next]: manifest.minAppVersion });
  for (const p of ["plugin/package.json", "plugin/package-lock.json"]) {
    const j = readJson(p);
    j.version = next;
    if (j.packages?.[""]) j.packages[""].version = next;
    writeJson(p, j);
  }
  console.log(`версия ${next}; обновите Cargo.lock (cargo check) и закоммитьте`);
}

const [cmd, arg] = process.argv.slice(2);
if (cmd === "check") check(arg);
else if (cmd === "set" && arg) set(arg);
else {
  console.error("использование: version.mjs check [ТЕГ] | set X.Y.Z");
  process.exit(2);
}
