import assert from "node:assert/strict";
import { test } from "node:test";

import { type LogStore, SyncLog } from "../src/log.ts";

// Код плагина ставит таймеры через window (как требует Obsidian для всплывающих окон).
(globalThis as Record<string, unknown>).window ??= globalThis;

function memStore(
  initial: string | null = null,
): LogStore & { text: string | null; writes: number } {
  const s = {
    text: initial,
    writes: 0,
    read: async () => s.text,
    write: async (t: string) => {
      s.text = t;
      s.writes++;
    },
  };
  return s;
}

test("журнал переживает перезапуск: старые строки перед новыми", async () => {
  const store = memStore();
  const a = new SyncLog(store, 100 * 1024, 10);
  await a.load();
  a.add("первый запуск");
  await a.flush();
  const b = new SyncLog(store, 100 * 1024, 10);
  b.add("до загрузки");
  await b.load();
  b.add("второй запуск");
  assert.deepEqual(b.tail(), ["первый запуск", "до загрузки", "второй запуск"]);
  await b.flush();
  assert.equal(store.text, "первый запуск\nдо загрузки\nвторой запуск\n");
});

test("журнал: размер ограничен, пишется пачкой и только при новых строках", async () => {
  const store = memStore();
  const log = new SyncLog(store, 200, 20);
  await log.load();
  for (let i = 0; i < 100; i++) log.add(`строка ${i}`);
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(store.writes, 1, "одна запись на серию строк");
  assert.ok((store.text ?? "").length <= 200);
  assert.equal(log.tail(1)[0], "строка 99");
  await log.flush();
  assert.equal(store.writes, 1, "без новых строк файл не переписывается");
});

test("журнал: ошибка чтения и записи не мешает", async () => {
  const log = new SyncLog(
    {
      read: () => Promise.reject(new Error("нет доступа")),
      write: () => Promise.reject(new Error("диск полон")),
    },
    1024,
    5,
  );
  await log.load();
  log.add("x");
  await log.flush();
  assert.deepEqual(log.tail(), ["x"]);
});
