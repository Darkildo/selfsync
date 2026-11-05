// Бэкенд на Node fs: сбои ФС не должны выглядеть как отсутствие файла (#8),
// короткая запись — как записанный файл (#9), а символическая ссылка не выводит за
// пределы vault'а (#10).

import assert from "node:assert/strict";
import * as realFsp from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { Executor } from "../src/executor.ts";
import { NodeBackend } from "../src/io/node.ts";
import type { IoAction, IoResult } from "../src/types.ts";

const ANY = { kind: "any" } as const;

const bytes = (s: string): Uint8Array => new TextEncoder().encode(s);

/** Действие без `id` (по каждому варианту объединения). */
type WithoutId<A> = A extends unknown ? Omit<A, "id"> : never;

async function withVault(run: (root: string) => Promise<void>): Promise<void> {
  const root = await realFsp.mkdtemp(join(tmpdir(), "selfsync-node-"));
  try {
    await realFsp.mkdir(join(root, "vault"), { recursive: true });
    await run(root);
  } finally {
    await realFsp.rm(root, { recursive: true, force: true });
  }
}

/** `lstat` пути на `suffix` падает с ошибкой `code`. */
function lstatFails(suffix: string, code: string): typeof realFsp {
  return {
    ...realFsp,
    lstat: (async (...args: Parameters<typeof realFsp.lstat>) => {
      if (String(args[0]).endsWith(suffix)) throw Object.assign(new Error(code), { code });
      return realFsp.lstat(...args);
    }) as typeof realFsp.lstat,
  } as typeof realFsp;
}

test("Node list: файл, исчезнувший между readdir и lstat (ENOENT), пропускается", async () => {
  await withVault(async (root) => {
    await realFsp.writeFile(join(root, "vault", "a.md"), "a");
    await realFsp.writeFile(join(root, "vault", "b.md"), "b");
    const fs = new NodeBackend(join(root, "vault"), join(root, "store"), lstatFails("/b.md", "ENOENT"), { caseSensitive: true });
    assert.deepEqual(
      (await fs.list()).map((m) => m.path),
      ["a.md"],
    );
  });
});

test("Node list: другие ошибки lstat (EIO, EACCES) обрывают обход", async () => {
  await withVault(async (root) => {
    await realFsp.writeFile(join(root, "vault", "a.md"), "a");
    for (const code of ["EIO", "EACCES"]) {
      const fs = new NodeBackend(join(root, "vault"), join(root, "store"), lstatFails("/a.md", code), { caseSensitive: true });
      await assert.rejects(fs.list(), { code });
    }
  });
});

test("Node: запись дописывается при коротких write, ноль байт — ошибка до замены файла", async () => {
  await withVault(async (root) => {
    let limit = 1;
    const fsp = {
      ...realFsp,
      open: (async (...args: Parameters<typeof realFsp.open>) => {
        const h = await realFsp.open(...args);
        return new Proxy(h, {
          get(t, k) {
            if (k === "write") {
              return (buf: Uint8Array, off: number, len: number, pos: number) => t.write(buf, off, Math.min(len, limit), pos);
            }
            const v = Reflect.get(t, k) as unknown;
            return typeof v === "function" ? v.bind(t) : v;
          },
        });
      }) as typeof realFsp.open,
    } as typeof realFsp;
    const fs = new NodeBackend(join(root, "vault"), join(root, "store"), fsp, { caseSensitive: true });

    // По байту за вызов: файл и временный файл частями — целиком.
    await fs.writeAtomic("note.md", bytes("полное содержимое"), ANY);
    assert.equal(await realFsp.readFile(join(root, "vault", "note.md"), "utf8"), "полное содержимое");
    await fs.tempWrite("t", 0, bytes("0123"));
    await fs.tempWrite("t", 4, bytes("4567"));
    assert.deepEqual(await fs.tempRead("t", 0, 8), bytes("01234567"));

    // Запись не продвигается — ошибка, прежний файл на месте.
    limit = 0;
    await assert.rejects(fs.writeAtomic("note.md", bytes("новое"), ANY));
    assert.equal(await realFsp.readFile(join(root, "vault", "note.md"), "utf8"), "полное содержимое");
  });
});

test("Node: через символическую ссылку ничего не читается и не меняется (#10)", async () => {
  await withVault(async (root) => {
    const vault = join(root, "vault");
    const ext = join(root, "external");
    await realFsp.mkdir(ext);
    await realFsp.writeFile(join(ext, "w.md"), "outside");
    await realFsp.symlink(ext, join(vault, "linked"), "dir");
    await realFsp.symlink(join(ext, "w.md"), join(vault, "leaf.md"));
    await realFsp.writeFile(join(vault, "inside.md"), "inside");
    await realFsp.mkdir(join(root, "store", "tmp"), { recursive: true });
    await realFsp.writeFile(join(root, "store", "tmp", "dl-x"), "big");
    const fs = new NodeBackend(vault, join(root, "store"), realFsp, { caseSensitive: true });
    const ex = new Executor(fs, { request: () => Promise.reject(new Error("без сети")) }, { server: "" });
    let id = 0;
    const run = (a: WithoutId<IoAction>) => ex.perform({ id: ++id, ...a } as IoAction);

    // Листинг отдаёт ссылки признаком и не заходит в них.
    const listed = await fs.list();
    assert.deepEqual(
      listed.filter((m) => m.link).map((m) => m.path).sort(),
      ["leaf.md", "linked"],
    );
    assert.ok(!listed.some((m) => m.path.startsWith("linked/")));

    for (const path of ["linked", "linked/w.md", "leaf.md"]) {
      assert.deepEqual(await run({ type: "stat", path }), { type: "stat", meta: { path, size: 0, mtime: 0, dir: false, link: true } });
    }
    assert.deepEqual(await run({ type: "read", path: "linked/w.md", offset: 0, len: null }), { type: "notFound" });
    assert.deepEqual(await run({ type: "read", path: "leaf.md", offset: 0, len: null }), { type: "notFound" });

    const failed = (r: IoResult) => r.type === "failed" || r.type === "precondition";
    const data = bytes("remote");
    assert.ok(failed(await run({ type: "write", path: "linked/new.md", data, expect: { kind: "absent" } })));
    assert.ok(failed(await run({ type: "write", path: "linked/new.md", data, expect: { kind: "any" } })));
    assert.ok(failed(await run({ type: "write", path: "leaf.md", data, expect: { kind: "any" } })));
    assert.ok(failed(await run({ type: "commitTemp", temp: "dl-x", path: "linked/big.md", expect: { kind: "any" } })));
    assert.ok(failed(await run({ type: "mkdir", path: "linked/sub" })));
    assert.ok(failed(await run({ type: "rmdir", path: "linked" })));
    assert.ok(failed(await run({ type: "trash", path: "linked/w.md", expect: { kind: "any" } })));
    assert.ok(failed(await run({ type: "delete", path: "linked/w.md", expect: { kind: "any" } })));
    assert.ok(failed(await run({ type: "rename", from: "inside.md", to: "linked/inside.md" })));
    assert.ok(failed(await run({ type: "rename", from: "linked/w.md", to: "out.md" })));

    assert.deepEqual(await realFsp.readdir(ext), ["w.md"]);
    assert.equal(await realFsp.readFile(join(ext, "w.md"), "utf8"), "outside");
    assert.ok((await realFsp.lstat(join(vault, "linked"))).isSymbolicLink());
    assert.ok((await realFsp.lstat(join(vault, "leaf.md"))).isSymbolicLink());
    assert.equal(await realFsp.readFile(join(vault, "inside.md"), "utf8"), "inside");
  });
});
