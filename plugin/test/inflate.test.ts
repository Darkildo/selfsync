// Распаковка deflate (src/inflate.ts) против zlib из Node: все типы блоков и
// стратегии сжатия, оборванный и испорченный поток.

import assert from "node:assert/strict";
import { test } from "node:test";
import { constants, deflateRawSync } from "node:zlib";

import { inflateRaw } from "../src/inflate.ts";

function rand(n: number, seed: number): Buffer {
  const b = Buffer.alloc(n);
  let x = seed;
  for (let i = 0; i < n; i++) {
    x = (x * 1103515245 + 12345) >>> 0;
    b[i] = x >>> 24;
  }
  return b;
}

const text = Buffer.from(
  Array.from({ length: 4000 }, (_, i) => `строка ${i % 97}: synced ${"ab".repeat(i % 13)} — ${i}\n`).join(""),
);
const samples: [string, Buffer][] = [
  ["пусто", Buffer.alloc(0)],
  ["один байт", Buffer.from([42])],
  ["текст", text],
  ["случайные байты", rand(200_000, 7)],
  ["повторы с перекрытием", Buffer.from("a".repeat(70_000) + "ab".repeat(30_000))],
  ["смесь", Buffer.concat([rand(50_000, 3), text, rand(70_000, 9), text])],
];

test("распаковка совпадает с исходником на уровнях и стратегиях zlib", () => {
  const strategies = [constants.Z_DEFAULT_STRATEGY, constants.Z_FIXED, constants.Z_HUFFMAN_ONLY, constants.Z_RLE, constants.Z_FILTERED];
  for (const [name, data] of samples) {
    for (const level of [0, 1, 6, 9]) {
      for (const strategy of strategies) {
        const packed = deflateRawSync(data, { level, strategy });
        assert.ok(Buffer.from(inflateRaw(packed)).equals(data), `${name}, уровень ${level}, стратегия ${strategy}`);
      }
    }
  }
});

test("подсказка размера не влияет на результат", () => {
  const packed = deflateRawSync(text);
  for (const hint of [0, 1, text.length, text.length * 10]) {
    assert.ok(Buffer.from(inflateRaw(packed, hint)).equals(text));
  }
});

test("оборванный и испорченный поток — ошибка, а не мусор", () => {
  const packed = deflateRawSync(text, { level: 9 });
  for (const cut of [1, 10, packed.length >> 1, packed.length - 1]) {
    assert.throws(() => inflateRaw(packed.subarray(0, cut)), /inflate/, `обрезано до ${cut}`);
  }
  assert.throws(() => inflateRaw(Buffer.from([0x07])), /inflate: неверный тип блока/);
  // Блок без сжатия с неверной контрольной длиной.
  assert.throws(() => inflateRaw(Buffer.from([0x01, 0x05, 0x00, 0x00, 0x00])), /inflate/);
});
