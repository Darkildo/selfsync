// E2E: клиенты плагина (ядро в WASM + исполнитель на Node fs) против настоящего
// сервера. Obsidian не нужен: исполнитель и бэкенды — те же, что в плагине.

import assert from "node:assert/strict";
import * as fsp from "node:fs/promises";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";

import { buildRules, DEFAULT_CONFIG_SYNC } from "../src/configsync.ts";
import type { EngineConfig } from "../src/types.ts";
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

    // Ядро сообщает, что изменил цикл (уведомление synced).
    const synced = b.notices.filter((n) => n.kind === "synced");
    assert.ok(synced.some((n) => n.kind === "synced" && n.pulled.includes("notes/one.md")), JSON.stringify(synced));

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

  test("лимит неверных кодов: 429 с временем ожидания", async () => {
    // Свой сервер: блокировка адреса 127.0.0.1 не должна задеть соседние тесты.
    const own = await TestServer.start();
    try {
      const fresh = await TestClient.create(own, { name: "guess" });
      for (let i = 0; i < 10; i++) {
        const r = await fresh.runner.command({ type: "redeem", code: "aaaaaaaaaaaaaaaa", name: "guess" });
        assert.ok(r.type === "error" && r.code === "code_invalid", JSON.stringify(r));
      }
      const r = await fresh.runner.command({ type: "redeem", code: "aaaaaaaaaaaaaaaa", name: "guess" });
      assert.ok(r.type === "error" && r.code === "rate_limited", JSON.stringify(r));
      const wait = r.type === "error" ? (r.retryAfter ?? 0) : 0;
      assert.ok(wait > 800 && wait <= 900, String(wait));
      await fresh.stop();
    } finally {
      await own.stop();
    }
  });

  test("по ссылке только для чтения", async () => {
    const out = server.cli("link", "--vault", "rojoin", "--name", "tablet", "--url", server.url, "--read-only");
    assert.match(out, /только для чтения/);
    const code = /Код для ручного ввода: (\S+)/.exec(out)?.[1];
    assert.ok(code, out);
    const writer = await TestClient.create(server, { name: "writer", token: server.token("rojoin", "writer") });
    await writer.write("shared.md", "from writer\n");
    await writer.sync();
    const fresh = await TestClient.create(server, { name: "tablet" });
    const r = await fresh.runner.command({ type: "redeem", code, name: "tablet" });
    assert.ok(r.type === "token");
    const tablet = await TestClient.create(server, { name: "tablet", token: r.token });
    await tablet.write("mine.md", "local\n");
    await tablet.sync();
    assert.equal(await tablet.read("shared.md"), "from writer\n");
    assert.equal(tablet.status?.readOnly, true);
    assert.equal(tablet.status?.pending, 1);
    assert.ok(tablet.notices.some((n) => n.kind === "readOnly" && n.pending === 1));
    await writer.sync();
    assert.ok(!(await writer.files()).has("mine.md"));
    const j = await tablet.runner.command({ type: "createJoin", name: "x" });
    assert.ok(j.type === "error" && j.code === "read_only", JSON.stringify(j));
    // устройство с записью выписывает ro-ссылки и видит пометку в списке
    const j2 = await writer.runner.command({ type: "createJoin", name: "y", readOnly: true });
    assert.equal(j2.type, "join");
    const d = await writer.runner.command({ type: "devices" });
    assert.ok(d.type === "devices" && d.devices.some((x) => x.name === "tablet" && x.readOnly));
    await fresh.stop();
    await tablet.stop();
    await writer.stop();
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

describe("настройки", () => {
  test("шифрование: включение, пароль на втором устройстве, смена пароля", async () => {
    const [a, b] = await pair("ui-crypto");
    await a.write("secret.md", "top secret\n");
    await a.sync();
    await b.sync();
    a.runner.send({ type: "enableEncryption", password: "correct horse battery staple", remember: true });
    await a.sync();
    assert.ok(a.notices.some((n) => n.kind === "encryptionEnabled"), JSON.stringify(a.notices));
    assert.equal(a.status?.encrypted, true);
    assert.ok(a.rememberedKey, "ключ запомнен по просьбе пользователя");

    await b.write("from-b.md", "b\n");
    await b.sync();
    assert.equal(b.status?.state, "needPassword");
    b.runner.send({ type: "password", password: "correct horse battery staple", remember: false });
    await b.runner.idle();
    await b.sync();
    await a.sync();
    assert.equal(await a.read("from-b.md"), "b\n", "правка, сделанная до ввода пароля, дошла зашифрованной");

    a.runner.send({ type: "changePassword", old: "correct horse battery staple", new: "another long passphrase here" });
    await a.runner.idle();
    assert.ok(a.notices.some((n) => n.kind === "passwordChanged"));
    // Новое устройство входит уже с новым паролем.
    const c = await TestClient.create(server, { name: "c", token: server.token("ui-crypto", "c") });
    await c.sync();
    c.runner.send({ type: "password", password: "another long passphrase here", remember: false });
    await c.runner.idle();
    await c.sync();
    assert.equal(await c.read("secret.md"), "top secret\n");
    await a.stop();
    await b.stop();
    await c.stop();
  });

  test("смена ключа: перешифровка, пароль на другом устройстве, новая фраза", async () => {
    const PW = "correct horse battery staple";
    const [a, b] = await pair("ui-rekey");
    await a.write("secret.md", "top secret\n");
    await a.sync();
    a.runner.send({ type: "enableEncryption", password: PW, remember: true, recovery: true });
    await a.sync();
    b.runner.send({ type: "password", password: PW, remember: true });
    await b.runner.idle();
    await b.sync();
    const phrases = (): string[][] => a.notices.flatMap((n) => (n.kind === "recoveryPhrase" ? [n.words] : []));
    const oldWords = phrases()[0] ?? [];
    // неверный пароль — ответ сразу
    const bad = await a.runner.command({ type: "rekey", password: "wrong", remember: true });
    assert.ok(bad.type === "error" && bad.code === "wrong_password", JSON.stringify(bad));
    const ok = await a.runner.command({ type: "rekey", password: PW, remember: true });
    assert.equal(ok.type, "ok", JSON.stringify(ok));
    await a.sync();
    assert.ok(a.notices.some((n) => n.kind === "rekeyDone"), JSON.stringify(a.notices));
    assert.equal(phrases().length, 2, "новая фраза показана");
    // запомненный на b ключ — прежний: просит пароль, дальше как обычно
    await b.write("from-b.md", "b\n");
    await b.sync();
    assert.equal(b.status?.state, "needPassword");
    b.runner.send({ type: "password", password: PW, remember: true });
    await b.runner.idle();
    await b.sync();
    await a.sync();
    assert.equal(await a.read("from-b.md"), "b\n");
    assert.equal(await b.read("secret.md"), "top secret\n");
    // прежняя фраза больше не открывает vault
    const old = await b.runner.command({ type: "recover", phrase: oldWords.join(" "), newPassword: "x y z", remember: true });
    assert.ok(old.type === "error" && old.code === "wrong_phrase", JSON.stringify(old));
    await a.stop();
    await b.stop();
  });

  test("фраза восстановления: при включении, вход по фразе, удаление", async () => {
    const PW = "correct horse battery staple";
    const [a, b] = await pair("ui-recovery");
    await a.write("secret.md", "top secret\n");
    await a.sync();
    a.runner.send({ type: "enableEncryption", password: PW, remember: true, recovery: true });
    await a.sync();
    const shown = a.notices.filter((n) => n.kind === "recoveryPhrase");
    assert.equal(shown.length, 1, "фраза показана ровно один раз");
    const words = shown[0]?.kind === "recoveryPhrase" ? shown[0].words : [];
    assert.equal(words.length, 24);
    assert.equal(a.status?.recovery, true);
    // фраза не попала ни в журнал, ни в хранилище плагина (индекс, кэш)
    const sample = words.slice(0, 4).join(" ");
    assert.ok(!a.logs.some((l) => l.includes(sample)), "фраза в журнале");
    for (const f of await fsp.readdir(a.store, { recursive: true })) {
      const st = await fsp.stat(join(a.store, f));
      if (!st.isFile()) continue;
      const text = (await fsp.readFile(join(a.store, f))).toString("latin1");
      assert.ok(!text.includes(sample), `фраза в ${f}`);
    }
    // устройство без пароля входит фразой с новым паролем
    await b.sync();
    assert.equal(b.status?.state, "needPassword");
    const bad = await b.runner.command({ type: "recover", phrase: [...words.slice(1), words[0]].join(" "), newPassword: "x", remember: true });
    assert.equal(bad.type, "error");
    const ok = await b.runner.command({ type: "recover", phrase: words.join(" "), newPassword: "the new password", remember: true });
    assert.equal(ok.type, "ok", JSON.stringify(ok));
    await b.sync();
    assert.equal(await b.read("secret.md"), "top secret\n");
    // a с запомненным ключом продолжает работать
    await b.write("after.md", "after recovery\n");
    await b.sync();
    await a.sync();
    assert.equal(await a.read("after.md"), "after recovery\n");
    // новая фраза отменяет старую, удаление убирает
    const created = await a.runner.command({ type: "createRecovery", password: "the new password" });
    assert.equal(created.type, "recoveryPhrase");
    const old = await b.runner.command({ type: "recover", phrase: words.join(" "), newPassword: "y", remember: true });
    assert.ok(old.type === "error" && old.code === "wrong_phrase", JSON.stringify(old));
    const removed = await a.runner.command({ type: "removeRecovery", password: "the new password" });
    assert.equal(removed.type, "ok");
    await a.sync();
    assert.equal(a.status?.recovery, false);
    await a.stop();
    await b.stop();
  });

  test("устройства: список и отзыв; окно хранения; ссылка подключения и QR", async () => {
    const [a, b] = await pair("ui-devices");
    await a.sync();
    await b.sync();
    const list = await a.runner.command({ type: "devices" });
    assert.ok(list.type === "devices");
    const devs = list.type === "devices" ? list.devices : [];
    assert.deepEqual(devs.map((d) => d.name).sort(), ["a", "b"]);
    assert.equal(devs.filter((d) => d.current).length, 1);
    const bId = devs.find((d) => d.name === "b")?.id ?? -1;
    assert.deepEqual(await a.runner.command({ type: "revokeDevice", id: bId }), { type: "ok" });
    await b.sync();
    assert.equal(b.status?.state, "blocked");

    assert.deepEqual(await a.runner.command({ type: "setRetention", days: 45 }), { type: "retention", days: 45 });
    assert.deepEqual(await a.runner.command({ type: "getRetention" }), { type: "retention", days: 45 });

    const join = await a.runner.command({ type: "createJoin", name: "tablet" });
    assert.equal(join.type, "join");
    if (join.type !== "join") return;
    const { qrModules } = await import("../pkg/selfsync_wasm.js");
    const m = qrModules(join.url);
    const width = m[0] ?? 0;
    assert.ok(width >= 21 && m.length === 1 + width * width, `ширина ${width}`);
    // Код из ссылки действительно подключает.
    const fresh = await TestClient.create(server, { name: "tablet" });
    const r = await fresh.runner.command({ type: "redeem", code: join.code, name: "tablet" });
    assert.equal(r.type, "token");
    await a.stop();
    await b.stop();
    await fresh.stop();
  });
});

describe("настройки Obsidian", () => {
  const ALL = { appearance: true, hotkeys: true, snippets: true, pluginList: true, pluginSettings: true, editor: true };
  /** Конфигурация ядра, как её собирает плагин. */
  const cfg = (presets: typeof DEFAULT_CONFIG_SYNC): EngineConfig => ({
    excludes: buildRules(".obsidian", presets, []),
    hardExcludes: [".obsidian/plugins/selfsync/", ".trash/"],
    noMerge: [".obsidian/**"],
  });
  const settingsFiles: Record<string, string> = {
    ".obsidian/hotkeys.json": '{"a":1}\n',
    ".obsidian/appearance.json": '{"theme":"obsidian"}\n',
    ".obsidian/themes/Minimal/theme.css": "body{}\n",
    ".obsidian/snippets/wide.css": ".x{}\n",
    ".obsidian/community-plugins.json": '["dataview"]\n',
    ".obsidian/plugins/dataview/main.js": "module.exports={}\n",
    ".obsidian/plugins/dataview/manifest.json": '{"id":"dataview"}\n',
    ".obsidian/plugins/dataview/data.json": '{"x":1}\n',
    ".obsidian/app.json": '{"vimMode":true}\n',
    // никогда
    ".obsidian/plugins/selfsync/data.json": '{"token":"ns_secret"}\n',
    ".obsidian/workspace.json": '{"layout":1}\n',
    ".obsidian/cache/x.lock": "1\n",
  };

  test("пресеты переносят выбранное, токен selfsync и раскладка окон не уходят никогда", async () => {
    const token = server.token("cfgsync", "a");
    const a = await TestClient.create(server, { name: "a", token, config: cfg(ALL) });
    const b = await TestClient.create(server, { name: "b", token: server.token("cfgsync", "b"), config: cfg(ALL) });
    // «Сервер глазами всего»: клиент без исключений видит всё, что на сервере.
    const all = await TestClient.create(server, { name: "all", token: server.token("cfgsync", "all"), config: { excludes: [], hardExcludes: [] } });
    for (const [p, text] of Object.entries(settingsFiles)) await a.write(p, text);
    await a.sync();
    await b.sync();
    await all.sync();
    const onServer = await all.files();
    for (const p of Object.keys(settingsFiles)) {
      const never = p.includes("/selfsync/") || p.endsWith("workspace.json") || p.includes("/cache/");
      assert.equal(onServer.has(p), !never, `${p} на сервере`);
      assert.equal(await b.read(p), never ? null : settingsFiles[p], `${p} у b`);
    }
    // выключенный пресет: файлы остаются и на сервере, и у второго устройства
    await a.restart({ ...cfg({ ...ALL, hotkeys: false }) });
    await a.write(".obsidian/hotkeys.json", '{"changed":"here only"}\n');
    await a.sync();
    await all.sync();
    assert.equal((await all.files()).get(".obsidian/hotkeys.json"), '{"a":1}\n');
    await b.sync();
    assert.equal(await b.read(".obsidian/hotkeys.json"), '{"a":1}\n');
    for (const c of [a, b, all]) await c.stop();
  });

  test("одновременная правка hotkeys.json: валидный JSON, без копий", async () => {
    const a = await TestClient.create(server, { name: "a", token: server.token("cfgmerge", "a"), config: cfg(ALL) });
    const b = await TestClient.create(server, { name: "b", token: server.token("cfgmerge", "b"), config: cfg(ALL) });
    await a.write(".obsidian/hotkeys.json", '{\n  "a": 1,\n  "b": 2\n}\n');
    await a.sync();
    await b.sync();
    await a.write(".obsidian/hotkeys.json", '{\n  "a": "from a",\n  "b": 2\n}\n');
    await new Promise((r) => setTimeout(r, 20));
    await b.write(".obsidian/hotkeys.json", '{\n  "a": 1,\n  "b": "from b"\n}\n');
    await a.sync();
    await b.sync();
    await a.sync();
    const ta = await a.read(".obsidian/hotkeys.json");
    assert.equal(ta, await b.read(".obsidian/hotkeys.json"));
    assert.deepEqual(JSON.parse(ta ?? ""), { a: 1, b: "from b" }, "побеждает более поздняя правка");
    for (const c of [a, b]) {
      const names = [...(await c.files()).keys()];
      assert.ok(!names.some((n) => n.includes("(conflict") || n.includes(" (")), names.join(", "));
      assert.ok(!c.notices.some((n) => n.kind === "conflict"));
    }
    for (const c of [a, b]) await c.stop();
  });
});
