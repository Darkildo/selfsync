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
}

test("main.js: синк при старте и по событиям vault'а", async () => {
  const vault = await fsp.mkdtemp(join(tmpdir(), "notesync-plugin-"));
  await fsp.writeFile(join(vault, "hello.md"), "from obsidian\n");
  const app = obsidian.stubApp(vault, { server: server.url, token: server.token("plugin", "obsidian"), deviceName: "stub" });
  const require = Module.createRequire(import.meta.url);
  const Plugin = (require(join(PLUGIN, "main.js")) as { default: new (a: unknown, m: unknown) => LoadedPlugin }).default;
  const plugin = new Plugin(app, { id: "notesync", dir: ".obsidian/plugins/notesync" });
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
  const stored = await fsp.readdir(join(vault, ".obsidian/plugins/notesync"));
  assert.ok(stored.includes("index.bin"), `${stored}`);
  await other.sync();
  assert.ok(!(await other.files()).has(".obsidian/plugins/notesync/index.bin"));

  plugin.onunload();
  await other.stop();
  await fsp.rm(vault, { recursive: true, force: true });
});
