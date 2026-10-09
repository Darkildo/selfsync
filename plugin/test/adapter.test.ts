// Мобильный бэкенд (vault.adapter) на заглушке с регистронезависимой ФС против
// Node-клиента и настоящего сервера.

import assert from "node:assert/strict";
import * as fsp from "node:fs/promises";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { AdapterBackend } from "../src/io/adapter.ts";
import { TestClient, TestServer } from "./harness.ts";
import { StubAdapter } from "./stub-adapter.ts";

let server: TestServer;

before(async () => {
  server = await TestServer.start();
});

after(async () => {
  await server.stop();
});

const PLUGIN_DIR = ".obsidian/plugins/selfsync";

async function mobileAndDesktop(vault: string, stub = new StubAdapter(true)): Promise<[TestClient, StubAdapter, TestClient]> {
  const phone = await TestClient.create(server, {
    name: "phone",
    token: server.token(vault, "phone"),
    backend: () => new AdapterBackend(stub, PLUGIN_DIR, "system"),
    config: { caseInsensitive: true, hardExcludes: [".obsidian/"] },
  });
  const desk = await TestClient.create(server, { name: "desk", token: server.token(vault, "desk") });
  return [phone, stub, desk];
}

test("адаптер: файлы, смена регистра, удаление в корзину", async () => {
  const [phone, stub, desk] = await mobileAndDesktop("adapter");
  stub.put("Note.md", "hello\n");
  await phone.sync();
  await desk.sync();
  assert.equal(await desk.read("Note.md"), "hello\n");
  assert.ok(!stub.names().some((n) => n.endsWith(".selfsync-tmp")), "временных файлов не осталось");

  await fsp.rename(join(desk.vault, "Note.md"), join(desk.vault, "note.md"));
  await desk.sync();
  await phone.sync();
  assert.deepEqual(stub.names().filter((n) => !n.startsWith(".obsidian")), ["note.md"]);

  await fsp.rm(join(desk.vault, "note.md"));
  await desk.sync();
  await phone.sync();
  assert.deepEqual(stub.trashed, ["note.md"]);
  await phone.stop();
  await desk.stop();
});

test("адаптер: большой файл скачивается частями и дописывается appendBinary", async () => {
  const [phone, stub, desk] = await mobileAndDesktop("adapter-big");
  const size = 12 * 1024 * 1024;
  const data = new Uint8Array(size);
  for (let i = 0; i < size; i++) data[i] = (i * 40503) >>> 8;
  await desk.write("media/big.bin", data);
  await desk.sync();
  await phone.sync();
  const got = new Uint8Array(await stub.readBinary("media/big.bin"));
  assert.equal(got.length, size);
  assert.ok(Buffer.from(got).equals(Buffer.from(data)));
  assert.equal(stub.rewrites, 0, "временный файл не переписывался целиком");
  await phone.stop();
  await desk.stop();
});

test("адаптер: Obsidian до 1.12.3 (без appendBinary) докачивает перезаписью", async () => {
  const old = new StubAdapter(true);
  Object.defineProperty(old, "appendBinary", { value: undefined });
  const [phone, stub, desk] = await mobileAndDesktop("adapter-old", old);
  const size = 9 * 1024 * 1024;
  const data = new Uint8Array(size);
  for (let i = 0; i < size; i++) data[i] = (i * 2654435761) >>> 24;
  await desk.write("media/old.bin", data);
  await desk.sync();
  await phone.sync();
  const got = new Uint8Array(await stub.readBinary("media/old.bin"));
  assert.ok(Buffer.from(got).equals(Buffer.from(data)));
  assert.ok(stub.rewrites > 0, "без appendBinary временный файл переписывается");
  await phone.stop();
  await desk.stop();
});

test("адаптер: конфликт сохраняет обе версии, индекс переживает перезапуск", async () => {
  const [phone, stub, desk] = await mobileAndDesktop("adapter-conflict");
  stub.put("c.md", "a\nb\nc\n");
  await phone.sync();
  await desk.sync();
  stub.put("c.md", "a\nphone\nc\n");
  await desk.write("c.md", "a\ndesk\nc\n");
  await desk.sync();
  await phone.restart({ caseInsensitive: true, hardExcludes: [".obsidian/"] });
  await phone.sync();
  const names = stub.names().filter((n) => !n.startsWith(".obsidian")).sort();
  assert.equal(names.length, 2, `${names}`);
  assert.equal(stub.text("c.md"), "a\ndesk\nc\n");
  const copy = names.find((n) => n.includes("conflict"));
  assert.ok(copy);
  assert.equal(stub.text(copy), "a\nphone\nc\n");
  await phone.stop();
  await desk.stop();
});
