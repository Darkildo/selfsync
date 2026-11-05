// Гонка записи: пользователь сохраняет файл между проверкой условия и заменой его
// серверной версией. Правка не должна пропасть: либо запись не состоится, либо
// заменённая версия вернётся ядру (`displaced`) и станет копией конфликта.
// И правка того же размера с восстановленным mtime (`cp -p`, `rsync -t`).

import assert from "node:assert/strict";
import * as fsp from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";

import { Executor } from "../src/executor.ts";
import { AdapterBackend } from "../src/io/adapter.ts";
import type { HttpBackend } from "../src/io/backend.ts";
import { NodeBackend } from "../src/io/node.ts";
import type { IoResult } from "../src/types.ts";
import { TestClient, TestServer } from "./harness.ts";
import { StubAdapter } from "./stub-adapter.ts";

const EDIT = "NEW UNSYNCED USER EDIT";
const enc = (s: string) => new TextEncoder().encode(s);
const noHttp: HttpBackend = { request: () => Promise.reject(new Error("no http")) };
type Fsp = typeof fsp;

/** fsp, у которого перед заменой `target()` временным файлом выполняется `hook`. */
function racingFs(target: () => string | null, hook: (dst: string) => Promise<void>, base: Fsp = fsp): Fsp {
  return {
    ...base,
    rename: async (from: string, to: string) => {
      if (from.endsWith(".selfsync-tmp") && to === target()) await hook(to);
      return base.rename(from, to);
    },
  } as Fsp;
}

/** ФС без жёстких ссылок (FAT, часть сетевых). */
const noLinkFs = { ...fsp, link: () => Promise.reject(Object.assign(new Error("EPERM"), { code: "EPERM" })) } as unknown as Fsp;

async function walk(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const e of await fsp.readdir(dir, { withFileTypes: true }).catch(() => [])) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await walk(p)));
    else out.push(p);
  }
  return out;
}

async function vault(): Promise<{ base: string; vault: string; store: string }> {
  const base = await fsp.mkdtemp(join(tmpdir(), "selfsync-race-"));
  await fsp.mkdir(join(base, "vault"));
  await fsp.mkdir(join(base, "store"));
  return { base, vault: join(base, "vault"), store: join(base, "store") };
}

async function writeOver(exec: Executor, backend: { stat(p: string): Promise<{ size: number; mtime: number } | null> }, path: string, data: string, id = 1): Promise<IoResult> {
  const st = await backend.stat(path);
  assert.ok(st);
  return exec.perform({ type: "write", id, path, data: enc(data), expect: { kind: "stat", size: st.size, mtime: st.mtime } });
}

for (const [label, base] of [
  ["жёсткие ссылки", fsp],
  ["без жёстких ссылок", noLinkFs],
] as const) {
  describe(`NodeBackend, ${label}`, () => {
    test("правка до замены: запись не состоится, правка на месте", async () => {
      const v = await vault();
      const abs = join(v.vault, "race.md");
      const backend = new NodeBackend(v.vault, v.store, base, { caseSensitive: true });
      const exec = new Executor(backend, noHttp, { server: "" });
      await fsp.writeFile(abs, "base");
      const st = await backend.stat("race.md");
      assert.ok(st);
      // Сохранено после того, как ядро прочитало состояние, но до записи.
      await fsp.writeFile(abs, EDIT);
      const r = await exec.perform({ type: "write", id: 1, path: "race.md", data: enc("remote"), expect: { kind: "stat", size: st.size, mtime: st.mtime } });
      assert.equal(r.type, "precondition");
      assert.equal(await fsp.readFile(abs, "utf8"), EDIT);
      assert.deepEqual(await fsp.readdir(v.vault), ["race.md"], "временных файлов не осталось");

      // Обычная запись проходит и ничего не оставляет рядом.
      const ok = await writeOver(exec, backend, "race.md", "remote");
      assert.equal(ok.type, "stat");
      assert.equal(await fsp.readFile(abs, "utf8"), "remote");
      assert.deepEqual(await fsp.readdir(v.vault), ["race.md"]);
      await fsp.rm(v.base, { recursive: true, force: true });
    });

    test("файл появился перед созданием: он остаётся", async () => {
      const v = await vault();
      const abs = join(v.vault, "new.md");
      // Пользователь создаёт файл после проверки условия, вплотную перед тем, как
      // наш встаёт на место (link — для ФС со ссылками, rename — без них).
      const hook = async (to: unknown) => {
        if (to === abs) await fsp.writeFile(abs, EDIT);
      };
      const f = {
        ...base,
        link: async (from: string, to: string) => {
          await hook(to);
          return base.link(from, to);
        },
        rename: async (from: string, to: string) => {
          if (from.endsWith(".selfsync-tmp")) await hook(to);
          return base.rename(from, to);
        },
      } as Fsp;
      const backend = new NodeBackend(v.vault, v.store, f, { caseSensitive: true });
      const exec = new Executor(backend, noHttp, { server: "" });
      const r = await exec.perform({ type: "write", id: 1, path: "new.md", data: enc("remote"), expect: { kind: "absent" } });
      assert.equal(r.type, "precondition");
      assert.equal(await fsp.readFile(abs, "utf8"), EDIT);
      assert.deepEqual(await fsp.readdir(v.vault), ["new.md"]);
      await fsp.rm(v.base, { recursive: true, force: true });
    });
  });
}

describe("NodeBackend: правка в момент замены", () => {
  test("дописано в заменяемый файл — версия возвращается как вытесненная", async () => {
    const v = await vault();
    const abs = join(v.vault, "race.md");
    let armed = true;
    const f = racingFs(
      () => abs,
      async (dst) => {
        if (!armed) return;
        armed = false;
        // Редактор сохраняет файл на месте (тот же inode) прямо перед заменой.
        await fsp.writeFile(dst, EDIT);
      },
    );
    const backend = new NodeBackend(v.vault, v.store, f, { caseSensitive: true });
    const exec = new Executor(backend, noHttp, { server: "" });
    await fsp.writeFile(abs, "base");
    const r = await writeOver(exec, backend, "race.md", "remote");
    assert.equal(r.type, "displaced", JSON.stringify(r));
    assert.ok(r.type === "displaced");
    assert.equal(await fsp.readFile(abs, "utf8"), "remote");
    assert.equal(await fsp.readFile(join(v.vault, r.aside), "utf8"), EDIT);
    assert.ok(r.aside.endsWith(".selfsync-tmp"));
    await fsp.rm(v.base, { recursive: true, force: true });
  });
});

describe("AdapterBackend (мобильные)", () => {
  async function setup() {
    const stub = new StubAdapter(false);
    await stub.mkdir(".obsidian/plugins/selfsync");
    stub.put("race.md", "base");
    const backend = new AdapterBackend(stub, ".obsidian/plugins/selfsync", "local");
    const exec = new Executor(backend, noHttp, { server: "" });
    return { stub, backend, exec };
  }

  test("правка перед заменой: запись не состоится, правка на месте", async () => {
    const { stub, backend, exec } = await setup();
    const st = await backend.stat("race.md");
    assert.ok(st);
    const exists = stub.exists.bind(stub);
    let armed = true;
    stub.exists = async (p: string, sensitive?: boolean) => {
      if (armed && p === "race.md" && sensitive) {
        armed = false;
        stub.put("race.md", EDIT);
      }
      return exists(p, sensitive);
    };
    const r = await exec.perform({ type: "write", id: 1, path: "race.md", data: enc("remote"), expect: { kind: "stat", size: st.size, mtime: st.mtime } });
    assert.equal(armed, false);
    assert.equal(r.type, "precondition");
    assert.equal(stub.text("race.md"), EDIT);
    assert.deepEqual(stub.trashed, []);
    assert.deepEqual(
      stub.names().filter((n) => !n.startsWith(".obsidian/")),
      ["race.md"],
    );
  });

  test("сохранено заново, пока прежний отодвинут: новое остаётся", async () => {
    const { stub, backend, exec } = await setup();
    const rename = stub.rename.bind(stub);
    let armed = true;
    stub.rename = async (from: string, to: string) => {
      await rename(from, to);
      if (armed && from === "race.md") {
        armed = false;
        stub.put("race.md", EDIT);
      }
    };
    const r = await writeOver(exec, backend, "race.md", "remote");
    assert.equal(armed, false);
    assert.equal(r.type, "precondition");
    assert.equal(stub.text("race.md"), EDIT);
    assert.deepEqual(
      stub.names().filter((n) => !n.startsWith(".obsidian/")),
      ["race.md"],
      "отодвинутая известная версия убрана",
    );
  });

  test("обрыв перед последним переименованием: ничего не потеряно", async () => {
    const { stub, backend, exec } = await setup();
    stub.failRenameTo = (to) => to === "race.md";
    const r = await writeOver(exec, backend, "race.md", "remote");
    assert.equal(r.type, "failed");
    const texts = stub.names().map((n) => stub.text(n));
    assert.ok(texts.includes("base"), `прежняя версия цела: ${stub.names()}`);
    assert.ok(texts.includes("remote"), `новая записана целиком: ${stub.names()}`);
    assert.deepEqual(stub.trashed, []);
  });
});

describe("e2e: два клиента и сервер", () => {
  let server: TestServer;
  before(async () => {
    server = await TestServer.start();
  });
  after(async () => {
    await server.stop();
  });

  test("правка, сохранённая в момент скачивания, становится копией конфликта", async () => {
    let targetAbs: string | null = null;
    let armed = false;
    const a = await TestClient.create(server, {
      name: "a",
      token: server.token("race", "a"),
      backend: (vault, store) =>
        new NodeBackend(
          vault,
          store,
          racingFs(
            () => targetAbs,
            async (dst) => {
              if (!armed) return;
              armed = false;
              await fsp.writeFile(dst, EDIT);
            },
          ),
          { caseSensitive: true },
        ),
    });
    const b = await TestClient.create(server, { name: "b", token: server.token("race", "b") });
    await a.write("race.md", "base\n");
    await a.sync();
    await b.sync();
    await b.write("race.md", "remote from b\n");
    await b.sync();

    targetAbs = join(a.vault, "race.md");
    armed = true;
    await a.sync();
    assert.equal(armed, false, "гонка сработала");
    assert.equal(await a.read("race.md"), "remote from b\n");
    const conflicts = a.notices.filter((n) => n.kind === "conflict");
    assert.equal(conflicts.length, 1, JSON.stringify(a.notices));
    const files = [...(await a.files()).keys()].sort();
    assert.equal(files.length, 2, `копия рядом: ${files}`);
    assert.ok(files.every((f) => !f.endsWith(".selfsync-tmp")), `${files}`);
    const copy = files.find((f) => f !== "race.md") ?? "";
    assert.equal(await a.read(copy), EDIT);
    await a.sync();
    await b.sync();
    assert.equal(await b.read(copy), EDIT, "копия дошла до другого устройства");
    await a.stop();
    await b.stop();
  });

  /** Переписать байты, вернув прежний mtime (`touch -r`, `rsync -t`). */
  async function sameSizeKeepMtime(file: string, text: string): Promise<void> {
    const st = await fsp.stat(file, { bigint: true });
    await fsp.writeFile(file, text);
    // Наносекунды: utimes(Date) теряет их, и mtime мог бы сдвинуться на 1 мс.
    const sec = Number(st.mtimeNs / 1_000_000_000n);
    const t = sec + Number(st.mtimeNs % 1_000_000_000n) / 1e9;
    await fsp.utimes(file, t, t);
    const after = await fsp.stat(file);
    assert.equal(after.size, Number(st.size));
    assert.equal(Math.trunc(after.mtimeMs), Number(st.mtimeMs), "mtime тот же");
  }

  test("правка того же размера с прежним mtime уходит на сервер", async () => {
    const a = await TestClient.create(server, { name: "a", token: server.token("meta", "a") });
    const b = await TestClient.create(server, { name: "b", token: server.token("meta", "b") });
    await a.write("note.md", "AAAA\n");
    // Целая секунда: utimes тогда восстанавливает mtime без потерь.
    const file = join(a.vault, "note.md");
    const t = Math.floor((await fsp.stat(file)).mtimeMs / 1000);
    await fsp.utimes(file, t, t);
    await a.sync();
    await b.sync();
    assert.equal(await b.read("note.md"), "AAAA\n");

    await sameSizeKeepMtime(file, "BBBB\n");
    a.runner.send({ type: "changed", path: "note.md" });
    await a.runner.idle();
    await a.sync();
    await b.sync();
    assert.equal(await b.read("note.md"), "BBBB\n");

    // И без события — полным обходом после перезапуска.
    await sameSizeKeepMtime(file, "CCCC\n");
    await a.restart();
    await a.sync();
    await b.sync();
    assert.equal(await b.read("note.md"), "CCCC\n");
    await a.stop();
    await b.stop();
  });

  test("незамеченная правка не затирается чужой: обе сохраняются", async () => {
    const a = await TestClient.create(server, { name: "a", token: server.token("meta2", "a") });
    const b = await TestClient.create(server, { name: "b", token: server.token("meta2", "b") });
    await a.write("note.md", "1\n2\n3\nAAAA\n");
    const file = join(a.vault, "note.md");
    const t = Math.floor((await fsp.stat(file)).mtimeMs / 1000);
    await fsp.utimes(file, t, t);
    await a.sync();
    await b.sync();

    // Правка на A без события (A её ещё не видел), затем чужая на B.
    await sameSizeKeepMtime(file, "1\n2\n3\nBBBB\n");
    await b.write("note.md", "from b\n2\n3\nAAAA\n");
    await b.sync();
    await a.sync();
    await b.sync();
    const all = [...(await a.files()).values()].join("\n");
    assert.ok(all.includes("BBBB"), `правка A на месте: ${all}`);
    assert.ok(all.includes("from b"), `правка B на месте: ${all}`);
    await a.stop();
    await b.stop();
  });
});
