// Runner при сбое ядра: движок на заглушке, без WASM.

import assert from "node:assert/strict";
import { test } from "node:test";

import { type EngineLike, Runner } from "../src/engine.ts";
import type { Executor } from "../src/executor.ts";
import type { EngineEvent } from "../src/types.ts";

(globalThis as Record<string, unknown>).window ??= globalThis;

function fakeEngine(
  handle: (ev: EngineEvent) => unknown,
): EngineLike & { freed: boolean } {
  return {
    freed: false,
    handle: (_now: number, ev: EngineEvent) => handle(ev),
    nextWake: () => undefined,
    status: () => ({}),
    hasPending: () => false,
    free() {
      this.freed = true;
    },
  };
}

const noExec = {} as Executor;

test("Runner: паника ядра (ловушка WASM) — стоп и fatal, ждущая команда получает ошибку", async () => {
  let fatal = 0;
  const engine = fakeEngine((ev) => {
    if (ev.type === "command") return [];
    throw new WebAssembly.RuntimeError("unreachable");
  });
  const r = new Runner(
    {},
    null,
    noExec,
    { fatal: () => fatal++ },
    Date.now,
    () => engine,
  );
  const pending = r.command({ type: "listDeleted" });
  r.send({ type: "tick" });
  assert.equal(fatal, 1);
  assert.ok(r.isStopped);
  assert.ok(engine.freed);
  assert.equal((await pending).type, "error");
  // После остановки события не доходят и fatal не повторяется.
  r.send({ type: "tick" });
  assert.equal(fatal, 1);
});

test("Runner: команду с негодными данными ядро отвергает без остановки", async () => {
  let fatal = 0;
  const r = new Runner(
    {},
    null,
    noExec,
    { fatal: () => fatal++ },
    Date.now,
    () =>
      fakeEngine((ev) => {
        if (ev.type === "command")
          throw new Error("invalid type: map, expected string");
        return [];
      }),
  );
  const res = await r.command({ type: "listDeleted" });
  assert.equal(res.type, "error");
  assert.equal(fatal, 0);
  assert.ok(!r.isStopped);
  r.stop();
});

test("Runner: сбой самого free после паники не мешает остановке", () => {
  const engine = fakeEngine(() => {
    throw new WebAssembly.RuntimeError("unreachable");
  });
  engine.free = () => {
    throw new WebAssembly.RuntimeError("unreachable");
  };
  let fatal = 0;
  const r = new Runner(
    {},
    null,
    noExec,
    { fatal: () => fatal++ },
    Date.now,
    () => engine,
  );
  r.send({ type: "tick" });
  assert.equal(fatal, 1);
  assert.ok(r.isStopped);
});
