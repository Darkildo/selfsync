// Настоящее имя файла без чтения каталога на каждый stat (R4) и параллельный
// обход на мобильном (R5): считаем обращения к каталогам.

import assert from "node:assert/strict";
import * as realFsp from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { Executor } from "../src/executor.ts";
import { AdapterBackend, mapLimit } from "../src/io/adapter.ts";
import { DirNames, type HttpBackend } from "../src/io/backend.ts";
import { NodeBackend } from "../src/io/node.ts";
import type { IoAction } from "../src/types.ts";
import { StubAdapter } from "./stub-adapter.ts";

const ANY = { kind: "any" } as const;

const noHttp: HttpBackend = {
  request: () => Promise.reject(new Error("сеть в этом тесте не нужна")),
};

const bytes = (s: string): Uint8Array => new TextEncoder().encode(s);

function writes(n: number, dir: string): IoAction[] {
  return Array.from({ length: n }, (_, i) => ({
    type: "write" as const,
    id: i + 1,
    path: `${dir}/note-${i}.md`,
    data: bytes(`заметка ${i}`),
    expect: { kind: "absent" as const },
  }));
}

test("Node, регистронезависимая ФС: скачивание N файлов в папку не читает её на каждый файл", async () => {
  const root = await realFsp.mkdtemp(join(tmpdir(), "selfsync-dirnames-"));
  try {
    await realFsp.mkdir(join(root, "vault", "notes"), { recursive: true });
    let readdirs = 0;
    const fsp = {
      ...realFsp,
      readdir: ((...args: Parameters<typeof realFsp.readdir>) => {
        readdirs++;
        return realFsp.readdir(...args);
      }) as typeof realFsp.readdir,
    };
    const fs = new NodeBackend(join(root, "vault"), join(root, "store"), fsp, {
      caseSensitive: false,
    });
    const exec = new Executor(fs, noHttp, { server: "http://x" });
    const n = 50;
    for (const a of writes(n, "notes")) {
      const r = await exec.perform(a);
      assert.equal(r.type, "stat", JSON.stringify(r));
      assert.equal(
        r.type === "stat" && r.meta?.path,
        a.type === "write" ? a.path : "",
      );
    }
    // Раньше: 3–4 чтения каталога на файл; теперь листинг живёт ttl и пополняется.
    assert.ok(readdirs <= 3, `чтений каталога: ${readdirs} на ${n} файлов`);
  } finally {
    await realFsp.rm(root, { recursive: true, force: true });
  }
});

test("Node: настоящее имя после своего переименования — сразу, чужое — после ttl", async () => {
  const root = await realFsp.mkdtemp(join(tmpdir(), "selfsync-dirnames-"));
  try {
    const vault = join(root, "vault");
    await realFsp.mkdir(vault, { recursive: true });
    const fs = new NodeBackend(vault, join(root, "store"), realFsp, {
      caseSensitive: false,
    });
    await fs.writeAtomic("a.md", bytes("x"), ANY);
    assert.equal((await fs.stat("a.md"))?.path, "a.md");
    await fs.rename("a.md", "b.md");
    assert.equal(await fs.stat("a.md"), null);
    assert.equal((await fs.stat("b.md"))?.path, "b.md");
  } finally {
    await realFsp.rm(root, { recursive: true, force: true });
  }
});

test("DirNames: свои изменения видны сразу, смена регистра перечитывает каталог", async () => {
  let now = 0;
  let loads = 0;
  const names = new DirNames(2000, () => now);
  const load = async (): Promise<string[]> => {
    loads++;
    return ["Note.md", "sub"];
  };
  assert.deepEqual(await names.get("", load), ["Note.md", "sub"]);
  names.added("new.md");
  names.added("sub/deep/x.md");
  assert.deepEqual(await names.get("", load), ["Note.md", "sub", "new.md"]);
  assert.equal(loads, 1);
  names.removed("new.md");
  assert.deepEqual(await names.get("", load), ["Note.md", "sub"]);
  // Запись, отличная только регистром: какое имя осталось, решает ФС.
  names.added("note.md");
  await names.get("", load);
  assert.equal(loads, 2);
  // Чужие правки — после ttl.
  now += 2500;
  await names.get("", load);
  assert.equal(loads, 3);
});

test("адаптер: stat в большой папке и запись файлов не читают папку каждый раз", async () => {
  const stub = new StubAdapter(true);
  let lists = 0;
  const list = stub.list.bind(stub);
  stub.list = (p: string) => {
    lists++;
    return list(p);
  };
  const fs = new AdapterBackend(stub, ".obsidian/plugins/selfsync");
  const exec = new Executor(fs, noHttp, { server: "http://x" });
  await fs.mkdir("notes");
  for (const a of writes(40, "notes"))
    assert.equal((await exec.perform(a)).type, "stat");
  assert.ok(lists <= 3, `чтений папки: ${lists}`);
  // Обход видит все файлы, в детерминированном порядке.
  const all = (await fs.list()).filter((m) => !m.dir).map((m) => m.path);
  assert.equal(all.length, 40);
  assert.deepEqual(
    all,
    (await fs.list()).filter((m) => !m.dir).map((m) => m.path),
  );
});

test("mapLimit: не больше limit одновременно, порядок результата как у входа", async () => {
  let active = 0;
  let peak = 0;
  const items = Array.from({ length: 30 }, (_, i) => i);
  const out = await mapLimit(items, 8, async (x) => {
    active++;
    peak = Math.max(peak, active);
    await new Promise((r) => setTimeout(r, (x * 7) % 5));
    active--;
    return x * 2;
  });
  assert.deepEqual(
    out,
    items.map((x) => x * 2),
  );
  assert.ok(peak <= 8 && peak > 1, `одновременно: ${peak}`);
});
