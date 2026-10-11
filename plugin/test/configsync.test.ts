// Сборка итоговых правил из пресетов и перенос старых настроек.

import assert from "node:assert/strict";
import { test } from "node:test";

import { ALWAYS_EXCLUDED, buildRules, DEFAULT_CONFIG_SYNC, migrateUserExcludes } from "../src/configsync.ts";

test("по умолчанию каталог настроек исключён целиком, как в 0.1.x", () => {
  const r = buildRules(".obsidian", DEFAULT_CONFIG_SYNC, []);
  assert.equal(r[0], ".obsidian/");
  assert.ok(!r.some((l) => l.startsWith("!")));
  assert.equal(r.length, 1 + ALWAYS_EXCLUDED.length);
});

test("пресеты включают свои файлы, служебные исключены после, пользовательские — в конце", () => {
  const r = buildRules("cfg", { ...DEFAULT_CONFIG_SYNC, hotkeys: true, pluginList: true }, ["!cfg/workspace.json", "big/"]);
  assert.ok(r.includes("!cfg/hotkeys.json"));
  assert.ok(r.includes("!cfg/plugins/*/main.js"));
  assert.ok(!r.includes("!cfg/plugins/*/data.json"));
  assert.ok(r.indexOf("cfg/workspace.json") > r.indexOf("!cfg/hotkeys.json"));
  assert.deepEqual(r.slice(-2), ["!cfg/workspace.json", "big/"]);
});

test("старые настройки: строка каталога настроек уходит из пользовательских", () => {
  assert.deepEqual(migrateUserExcludes(".obsidian", [".obsidian/", "private/", " .obsidian "]), ["private/"]);
  assert.deepEqual(migrateUserExcludes("cfg", [".obsidian/"]), [".obsidian/"]);
});
