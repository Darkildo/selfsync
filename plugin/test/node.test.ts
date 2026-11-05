// Бэкенд на Node fs: сбои ФС не должны выглядеть как отсутствие файла (#8), а
// короткая запись — как записанный файл (#9).

import assert from "node:assert/strict";
import * as realFsp from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { NodeBackend } from "../src/io/node.ts";

const bytes = (s: string): Uint8Array => new TextEncoder().encode(s);

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
    await fs.writeAtomic("note.md", bytes("полное содержимое"));
    assert.equal(await realFsp.readFile(join(root, "vault", "note.md"), "utf8"), "полное содержимое");
    await fs.tempWrite("t", 0, bytes("0123"));
    await fs.tempWrite("t", 4, bytes("4567"));
    assert.deepEqual(await fs.tempRead("t", 0, 8), bytes("01234567"));

    // Запись не продвигается — ошибка, прежний файл на месте.
    limit = 0;
    await assert.rejects(fs.writeAtomic("note.md", bytes("новое")));
    assert.equal(await realFsp.readFile(join(root, "vault", "note.md"), "utf8"), "полное содержимое");
  });
});
