// E2E: клиенты плагина (ядро в WASM + исполнитель на Node fs) против настоящего
// сервера. Obsidian не нужен: исполнитель и бэкенды — те же, что в плагине.

import assert from "node:assert/strict";
import * as fsp from "node:fs/promises";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";

import { TestClient, TestServer } from "./harness.ts";

let server: TestServer;

before(async () => {
  server = await TestServer.start();
});

after(async () => {
  await server.stop();
});

async function pair(vault: string): Promise<[TestClient, TestClient]> {
  const a = await TestClient.create(server, { name: "a", token: server.token(vault, "a") });
  const b = await TestClient.create(server, { name: "b", token: server.token(vault, "b") });
  return [a, b];
}

describe("синк между двумя устройствами", () => {
  test("новый файл, правка, удаление", async () => {
    const [a, b] = await pair("basic");
    await a.write("notes/one.md", "# one\nfirst\n");
    await a.sync();
    await b.sync();
    assert.equal(await b.read("notes/one.md"), "# one\nfirst\n");

    await b.write("notes/one.md", "# one\nfirst\nfrom b\n");
    await b.sync();
    await a.sync();
    assert.equal(await a.read("notes/one.md"), "# one\nfirst\nfrom b\n");

    await fsp.rm(join(a.vault, "notes/one.md"));
    await a.sync();
    await b.sync();
    assert.equal(await b.read("notes/one.md"), null, "удаление дошло");
    const trashed = await fsp.readdir(join(b.store, "trash"));
    assert.equal(trashed.length, 1, "удалённое ушло в корзину, а не стёрто");
    await a.stop();
    await b.stop();
  });

  test("правки в разных местах сливаются, пересекающиеся — копией", async () => {
    const [a, b] = await pair("merge");
    await a.write("m.md", "1\n2\n3\n4\n5\n");
    await a.sync();
    await b.sync();
    await a.write("m.md", "1 a\n2\n3\n4\n5\n");
    await b.write("m.md", "1\n2\n3\n4\n5 b\n");
    await a.sync();
    await b.sync();
    await a.sync();
    assert.equal(await a.read("m.md"), "1 a\n2\n3\n4\n5 b\n");
    assert.equal(await b.read("m.md"), "1 a\n2\n3\n4\n5 b\n");

    await a.write("m.md", "1 a\n2\nthree from a\n4\n5 b\n");
    await b.write("m.md", "1 a\n2\nthree from b\n4\n5 b\n");
    await a.sync();
    await b.sync();
    await a.sync();
    const files = [...(await b.files()).keys()].sort();
    assert.equal(files.length, 2, `${files}`);
    const copy = files.find((f) => f.includes("conflict"));
    assert.ok(copy, `${files}`);
    assert.equal(await b.read(copy), "1 a\n2\nthree from b\n4\n5 b\n");
    assert.equal(b.notices.filter((n) => n.kind === "conflict").length, 1);
    await a.stop();
    await b.stop();
  });

  test("переименование и правка на другом устройстве", async () => {
    const [a, b] = await pair("rename");
    await a.write("old.md", "l1\nl2\nl3\n");
    await a.sync();
    await b.sync();
    await fsp.rename(join(a.vault, "old.md"), join(a.vault, "new.md"));
    a.runner.send({ type: "renamed", from: "old.md", to: "new.md" });
    await a.sync();
    await b.write("old.md", "l1\nl2\nl3\nfrom b\n");
    await b.sync();
    await a.sync();
    await b.sync();
    assert.equal(await a.read("new.md"), "l1\nl2\nl3\nfrom b\n");
    assert.equal(await b.read("new.md"), "l1\nl2\nl3\nfrom b\n");
    assert.equal(await b.read("old.md"), null);
    await a.stop();
    await b.stop();
  });

  test("большой файл идёт частями и докачивается после перезапуска", async () => {
    const [a, b] = await pair("big");
    const size = 20 * 1024 * 1024;
    const data = new Uint8Array(size);
    for (let i = 0; i < size; i++) data[i] = (i * 2654435761) >>> 24;
    await a.write("big.bin", data);
    await a.sync();
    // Скачивание обрывается перезапуском посреди передачи.
    b.runner.send({ type: "syncNow" });
    await new Promise((r) => setTimeout(r, 30));
    await b.restart();
    await b.sync();
    const got = await fsp.readFile(join(b.vault, "big.bin"));
    assert.equal(got.length, size);
    assert.ok(Buffer.from(data).equals(got), "содержимое совпадает");
    await a.stop();
    await b.stop();
  });
});

describe("подключение", () => {
  test("по одноразовому коду", async () => {
    const out = server.cli("link", "--vault", "join", "--name", "phone", "--url", server.url);
    const code = /Код для ручного ввода: (\S+)/.exec(out)?.[1];
    assert.ok(code, out);
    const fresh = await TestClient.create(server, { name: "phone" });
    const r = await fresh.runner.command({ type: "redeem", code, name: "phone" });
    assert.equal(r.type, "token");
    assert.ok(r.type === "token" && r.token.length > 20 && r.vault === "join");
    await fresh.stop();
  });

  test("отозванный токен останавливает синк", async () => {
    const token = server.token("revoke", "gone");
    const c = await TestClient.create(server, { name: "gone", token });
    server.cli("token", "revoke", "--name", "gone", "--vault", "revoke");
    await c.write("x.md", "x\n");
    await c.sync();
    assert.equal(c.status?.state, "blocked");
    assert.ok(c.notices.some((n) => n.kind === "unauthorized"));
    await c.stop();
  });
});

describe("команды окон", () => {
  test("конфликты: список и выбор своей версии", async () => {
    const [a, b] = await pair("ui-conflicts");
    await a.write("c.md", "x\ny\nz\n");
    await a.sync();
    await b.sync();
    await a.write("c.md", "x\nA\nz\n");
    await b.write("c.md", "x\nB\nz\n");
    await a.sync();
    await b.sync();
    const r = await b.runner.command({ type: "conflicts" });
    assert.equal(r.type, "conflicts");
    assert.ok(r.type === "conflicts" && r.items.length === 1);
    const c = r.type === "conflicts" ? r.items[0] : undefined;
    assert.ok(c);
    b.runner.send({ type: "resolve", id: c.id, choice: "keepMine" });
    await b.runner.idle();
    await b.sync();
    await a.sync();
    assert.equal(await a.read("c.md"), "x\nB\nz\n", "своя версия ушла на сервер");
    assert.equal(await b.read(c.copy), null, "копия убрана");
    const after = await b.runner.command({ type: "conflicts" });
    assert.ok(after.type === "conflicts" && after.items.length === 0);
    await a.stop();
    await b.stop();
  });

  test("корзина сервера: восстановление удалённого", async () => {
    const [a, b] = await pair("ui-deleted");
    await a.write("gone.md", "keep me\n");
    await a.sync();
    await b.sync();
    await fsp.rm(join(a.vault, "gone.md"));
    await a.sync();
    const list = await b.runner.command({ type: "listDeleted" });
    assert.ok(list.type === "deleted" && list.items.some((d) => d.path === "gone.md"), JSON.stringify(list));
    const r = await b.runner.command({ type: "restoreDeleted", paths: ["gone.md"] });
    assert.deepEqual(r, { type: "restored", count: 1 });
    await b.sync();
    await a.sync();
    assert.equal(await a.read("gone.md"), "keep me\n");
    await a.stop();
    await b.stop();
  });

  test("история: возврат старой ревизии", async () => {
    const [a, b] = await pair("ui-history");
    await a.write("h.md", "v1\n");
    await a.sync();
    await a.write("h.md", "v2\n");
    await a.sync();
    const h = await a.runner.command({ type: "history", path: "h.md" });
    assert.ok(h.type === "history" && h.revisions.length === 2, JSON.stringify(h));
    const oldest = h.type === "history" ? h.revisions[h.revisions.length - 1] : undefined;
    assert.ok(oldest);
    const r = await a.runner.command({ type: "restoreRevision", path: "h.md", rev: oldest.rev });
    assert.notEqual(r.type, "error", JSON.stringify(r));
    await a.sync();
    await b.sync();
    assert.equal(await a.read("h.md"), "v1\n");
    assert.equal(await b.read("h.md"), "v1\n");
    await a.stop();
    await b.stop();
  });
});
