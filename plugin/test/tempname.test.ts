// Имена временных файлов: укороченная база для имён на пределе 255 байт (#11).
// Хэш и база должны совпадать с ядром (crates/selfsync-core/src/tempname.rs).

import assert from "node:assert/strict";
import * as fsp from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { pathHash, TEMP_SUFFIX, tempBase } from "../src/io/backend.ts";
import { NodeBackend } from "../src/io/node.ts";

const bytes = (s: string): number => new TextEncoder().encode(s).length;

test("pathHash и tempBase совпадают с ядром", () => {
  assert.equal(pathHash(""), "cbf29ce484222325");
  assert.equal(pathHash("a.md"), "6ab57f8279f280cf");
  assert.equal(pathHash("dir/заметка.md"), "b521fc211dc46709");
  assert.equal(tempBase(`dir/${"я".repeat(126)}.md`), `dir/${"я".repeat(107)}~9e5f9fd5ffd34c3c`);
});

test("tempBase: короткие имена как есть, длинные укладываются в 255 байт с любым хвостом", () => {
  assert.equal(tempBase("a.md"), "a.md");
  assert.equal(tempBase("dir/a.md"), "dir/a.md");
  for (const name of [`${"a".repeat(252)}.md`, `${"я".repeat(126)}.md`, `${"😀".repeat(63)}.md`]) {
    const base = tempBase(`dir/${name}`);
    assert.ok(base.startsWith("dir/"));
    assert.ok(bytes(`${base.slice(4)}.case-99${TEMP_SUFFIX}`) <= 255, base);
  }
});

test("Node writeAtomic и корзина: имя на 255 байт", async () => {
  const root = await fsp.mkdtemp(join(tmpdir(), "selfsync-tempname-"));
  try {
    await fsp.mkdir(join(root, "vault"));
    const fs = new NodeBackend(join(root, "vault"), join(root, "store"), fsp, { caseSensitive: true });
    const name = `${"я".repeat(126)}.md`;
    await fs.writeAtomic(name, new TextEncoder().encode("x"));
    assert.deepEqual(await fsp.readdir(join(root, "vault")), [name]);
    await fs.trash(name);
    assert.deepEqual(await fsp.readdir(join(root, "vault")), []);
    assert.equal((await fsp.readdir(join(root, "store", "trash"))).length, 1);
  } finally {
    await fsp.rm(root, { recursive: true, force: true });
  }
});
