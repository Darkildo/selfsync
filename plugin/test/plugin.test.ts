// Собранный main.js целиком (WASM встроен) на заглушке Obsidian против настоящего
// сервера: загрузка плагина, синк при старте, события vault'а, статус-бар.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fsp from "node:fs/promises";
import Module from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { TestClient, TestServer } from "./harness.ts";
import * as obsidian from "./obsidian-stub.ts";

const PLUGIN = new URL("..", import.meta.url).pathname;

let server: TestServer;

before(async () => {
  const b = spawnSync("node", ["esbuild.config.mjs", "production"], { cwd: PLUGIN, encoding: "utf8" });
  assert.equal(b.status, 0, b.stderr);
  server = await TestServer.start();
  // main.js делает require("obsidian") — подставить заглушку.
  const m = Module as unknown as { _load: (req: string, ...rest: unknown[]) => unknown };
  const orig = m._load;
  m._load = function (req: string, ...rest: unknown[]) {
    if (req === "obsidian") return obsidian;
    return orig.call(this, req, ...rest);
  };
  (globalThis as Record<string, unknown>).document = { visibilityState: "visible" };
});

after(async () => {
  await server.stop();
});

interface LoadedPlugin {
  onload(): Promise<void>;
  onunload(): void;
  runner?: { idle(): Promise<void>; send(e: unknown): void };
  statusBar: { text: string };
  settingTab: { getSettingDefinitions(): { items: { render(s: unknown): unknown }[] }[] };
}

/** Кнопки и строки, которые рисует вкладка настроек (с ответами сервера). */
async function renderSettings(plugin: LoadedPlugin): Promise<obsidian.Setting[]> {
  obsidian.settings.length = 0;
  for (const g of plugin.settingTab.getSettingDefinitions()) {
    for (const it of g.items) it.render(new obsidian.Setting());
  }
  // Список устройств и окно хранения приходят ответом сервера.
  await plugin.runner?.idle();
  await new Promise((r) => setTimeout(r, 200));
  return obsidian.settings.filter((s) => !s.hidden);
}

async function loadPlugin(vault: string, token: string, extra: Record<string, unknown> = {}): Promise<LoadedPlugin> {
  const app = obsidian.stubApp(vault, { server: server.url, token, deviceName: "stub", ...extra });
  const require = Module.createRequire(import.meta.url);
  const Plugin = (require(join(PLUGIN, "main.js")) as { default: new (a: unknown, m: unknown) => LoadedPlugin }).default;
  const plugin = new Plugin(app, { id: "selfsync", dir: ".obsidian/plugins/selfsync" });
  await plugin.onload();
  for (let i = 0; i < 100 && !plugin.runner; i++) await new Promise((r) => setTimeout(r, 10));
  assert.ok(plugin.runner, "движок запущен");
  await plugin.runner.idle();
  return plugin;
}

test("main.js: синк при старте и по событиям vault'а", async () => {
  const vault = await fsp.mkdtemp(join(tmpdir(), "selfsync-plugin-"));
  await fsp.writeFile(join(vault, "hello.md"), "from obsidian\n");
  const app = obsidian.stubApp(vault, { server: server.url, token: server.token("plugin", "obsidian"), deviceName: "stub" });
  const require = Module.createRequire(import.meta.url);
  const Plugin = (require(join(PLUGIN, "main.js")) as { default: new (a: unknown, m: unknown) => LoadedPlugin }).default;
  const plugin = new Plugin(app, { id: "selfsync", dir: ".obsidian/plugins/selfsync" });
  await plugin.onload();
  // start() асинхронно читает индекс.
  for (let i = 0; i < 100 && !plugin.runner; i++) await new Promise((r) => setTimeout(r, 10));
  assert.ok(plugin.runner, "движок запущен");
  await plugin.runner.idle();
  assert.match(plugin.statusBar.text, /Синхронизировано/);

  const other = await TestClient.create(server, { name: "other", token: server.token("plugin", "other") });
  await other.sync();
  assert.equal(await other.read("hello.md"), "from obsidian\n");

  // Правка с другого устройства приходит по опросу/синку, своя — по событию modify.
  await other.write("from-other.md", "hi\n");
  await other.sync();
  await fsp.writeFile(join(vault, "hello.md"), "from obsidian\nedited\n");
  app.vault.emit("modify", { path: "hello.md" });
  plugin.runner.send({ type: "syncNow" });
  await plugin.runner.idle();
  assert.equal(await fsp.readFile(join(vault, "from-other.md"), "utf8"), "hi\n");
  await other.sync();
  assert.equal(await other.read("hello.md"), "from obsidian\nedited\n");

  // Индекс и кэш — в каталоге плагина, не в vault'е на сервере.
  const stored = await fsp.readdir(join(vault, ".obsidian/plugins/selfsync"));
  assert.ok(stored.includes("index.bin"), `${stored}`);
  await other.sync();
  assert.ok(!(await other.files()).has(".obsidian/plugins/selfsync/index.bin"));

  plugin.onunload();
  await other.stop();
  await fsp.rm(vault, { recursive: true, force: true });
});

test("main.js: устройство только для чтения — статус и скрытые действия", async () => {
  const roToken = server.cli("token", "add", "--vault", "ro", "--name", "mirror", "--read-only").trim().split("\n").pop() ?? "";
  const rwVault = await fsp.mkdtemp(join(tmpdir(), "selfsync-plugin-rw-"));
  const roVault = await fsp.mkdtemp(join(tmpdir(), "selfsync-plugin-ro-"));
  const rw = await loadPlugin(rwVault, server.token("ro", "writer"));
  const ro = await loadPlugin(roVault, roToken);
  await fsp.writeFile(join(roVault, "local.md"), "only here\n");
  ro.runner?.send({ type: "syncNow" });
  await ro.runner?.idle();
  assert.match(ro.statusBar.text, /Только чтение/);
  assert.match(ro.statusBar.text, /не уйдут на сервер: 1/);
  assert.ok(obsidian.shown.some((m) => m.includes("только для чтения")), obsidian.shown.join("\n"));

  const buttons = (rows: obsidian.Setting[]): string[] => rows.flatMap((r) => r.buttons);
  const rwRows = await renderSettings(rw);
  for (const b of ["Включить шифрование", "Создать ссылку", "Отозвать"]) assert.ok(buttons(rwRows).includes(b), `${b}: ${buttons(rwRows)}`);
  assert.ok(rwRows.some((r) => r.name === "Только чтение" && r.toggles === 1), "переключатель для ссылки");
  assert.ok(rwRows.some((r) => r.desc.includes("только чтение")), "пометка в списке устройств");
  const roRows = await renderSettings(ro);
  for (const b of ["Включить шифрование", "Создать ссылку", "Отозвать"]) assert.ok(!buttons(roRows).includes(b), `${b}: ${buttons(roRows)}`);
  assert.ok(roRows.some((r) => r.desc.includes("подключено только для чтения")), "пояснение");

  rw.onunload();
  ro.onunload();
  await fsp.rm(rwVault, { recursive: true, force: true });
  await fsp.rm(roVault, { recursive: true, force: true });
});

test("main.js: пришли настройки Obsidian — одно уведомление о перезапуске", async () => {
  const vault = await fsp.mkdtemp(join(tmpdir(), "selfsync-plugin-cfg-"));
  // Старые настройки (до пресетов): каталог настроек в пользовательских исключениях.
  const plugin = await loadPlugin(vault, server.token("cfgplugin", "obsidian"), {
    excludes: [".obsidian/"],
    configSync: undefined,
  });
  const p = plugin as unknown as { settings: { excludes: string[]; configSync: Record<string, boolean> }; saveSettings(o: unknown): Promise<void> };
  assert.deepEqual(p.settings.excludes, [], "строка каталога настроек перенесена в итоговые правила");
  p.settings.configSync.hotkeys = true;
  await p.saveSettings({ reconfigure: true });
  const other = await TestClient.create(server, {
    name: "other",
    token: server.token("cfgplugin", "other"),
    config: { excludes: [], hardExcludes: [] },
  });
  await other.write(".obsidian/hotkeys.json", '{"k":1}\n');
  await other.write(".obsidian/app.json", '{"x":1}\n');
  await other.sync();
  const before = obsidian.shown.length;
  plugin.runner?.send({ type: "syncNow" });
  await plugin.runner?.idle();
  assert.equal(await fsp.readFile(join(vault, ".obsidian/hotkeys.json"), "utf8"), '{"k":1}\n');
  await assert.rejects(fsp.readFile(join(vault, ".obsidian/app.json")), "app.json — другой пресет");
  const restart = (): number => obsidian.shown.slice(before).filter((m) => m.includes("Перезапустите Obsidian")).length;
  assert.equal(restart(), 1);
  await other.write(".obsidian/hotkeys.json", '{"k":2}\n');
  await other.sync();
  plugin.runner?.send({ type: "syncNow" });
  await plugin.runner?.idle();
  assert.equal(restart(), 1, "не чаще раза в сессию");
  plugin.onunload();
  await other.stop();
  await fsp.rm(vault, { recursive: true, force: true });
});
