// Чтение по частям на мобильном бэкенде: файл читается с устройства один раз за
// проход, а не на каждый кусок; запись сбрасывает кэш.

import assert from "node:assert/strict";
import { test } from "node:test";

import { AdapterBackend } from "../src/io/adapter.ts";
import { StubAdapter } from "./stub-adapter.ts";

class CountingAdapter extends StubAdapter {
  reads = 0;
  override async readBinary(p: string): Promise<ArrayBuffer> {
    this.reads++;
    return super.readBinary(p);
  }
}

const CHUNK = 4;

async function readAll(b: AdapterBackend, path: string): Promise<Uint8Array> {
  const parts: number[] = [];
  for (let off = 0; ; off += CHUNK) {
    const part = await b.read(path, off, CHUNK);
    assert.ok(part);
    parts.push(...part);
    if (part.length < CHUNK) break;
  }
  return Uint8Array.from(parts);
}

test("файл по частям читается с устройства один раз", async () => {
  const a = new CountingAdapter();
  const b = new AdapterBackend(a, ".obsidian/plugins/selfsync");
  const data = Uint8Array.from({ length: 30 }, (_, i) => i);
  await b.writeAtomic("big.bin", data);
  a.reads = 0;
  assert.deepEqual(await readAll(b, "big.bin"), data);
  assert.equal(a.reads, 1);
  // Дочитан до конца — кэш отпущен: следующий проход читает заново.
  await readAll(b, "big.bin");
  assert.equal(a.reads, 2);
});

test("изменённый файл не берётся из кэша", async () => {
  const a = new CountingAdapter();
  const b = new AdapterBackend(a, ".obsidian/plugins/selfsync");
  await b.writeAtomic("a.bin", Uint8Array.from({ length: 12 }, () => 1));
  assert.deepEqual(await b.read("a.bin", 0, CHUNK), Uint8Array.of(1, 1, 1, 1));
  // Файл переписан снаружи (другой плагин) — размер и mtime другие.
  await a.writeBinary("a.bin", Uint8Array.from({ length: 12 }, () => 2).buffer);
  assert.deepEqual(await b.read("a.bin", CHUNK, CHUNK), Uint8Array.of(2, 2, 2, 2));
  // Запись через бэкенд сбрасывает кэш.
  await b.writeAtomic("a.bin", Uint8Array.from({ length: 12 }, () => 3));
  assert.deepEqual(await b.read("a.bin", 0, CHUNK), Uint8Array.of(3, 3, 3, 3));
});

test("временный файл докачки читается по частям так же", async () => {
  const a = new CountingAdapter();
  const b = new AdapterBackend(a, ".obsidian/plugins/selfsync");
  const data = Uint8Array.from({ length: 20 }, (_, i) => 100 + i);
  await b.tempWrite("t", 0, data);
  a.reads = 0;
  const parts: number[] = [];
  for (let off = 0; off < 20; off += CHUNK) parts.push(...((await b.tempRead("t", off, CHUNK)) ?? []));
  assert.deepEqual(Uint8Array.from(parts), data);
  assert.equal(a.reads, 1);
  assert.equal(await b.read("missing.bin", 0, CHUNK), null);
});
