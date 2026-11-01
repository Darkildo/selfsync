// Передача ключа по коду: ссылка для QR, опрос и тест-векторы ядра
// (spec/vectors/handoff.json), сверенные WebCrypto — тем, чем считала бы любая
// надстройка без WASM. Без WASM: ядро здесь не загружается.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { connectLink, formatSas, pollHandoff } from "../src/handoff.ts";
import type { UiResult } from "../src/types.ts";

interface Vector {
  prefix: string;
  h: string;
  a_secret: string;
  b_secret: string;
  a_public: string;
  b_public: string;
  commit: string;
  link_secret: string;
  mac: string;
  sas: string;
  nonce: string;
  payload: string;
  wrapped: string;
}

const vectors = (JSON.parse(readFileSync(new URL("../../spec/vectors/handoff.json", import.meta.url), "utf8")) as { cases: Vector[] }).cases;
const subtle = globalThis.crypto.subtle;
const hex = (s: string): Uint8Array<ArrayBuffer> => new Uint8Array(Buffer.from(s, "hex"));
const toHex = (b: ArrayBuffer | Uint8Array): string => Buffer.from(b instanceof Uint8Array ? b : new Uint8Array(b)).toString("hex");
const b64u = (b: Uint8Array): string => Buffer.from(b).toString("base64url");
const cat = (...xs: Uint8Array[]): Uint8Array<ArrayBuffer> => new Uint8Array(Buffer.concat(xs));

/** Секрет пары в WebCrypto: только JWK (d вместе с x, y открытого ключа). */
async function privateKey(secret: string, pub: string): Promise<CryptoKey> {
  const p = hex(pub);
  const jwk = { kty: "EC", crv: "P-256", d: b64u(hex(secret)), x: b64u(p.slice(1, 33)), y: b64u(p.slice(33)), ext: true };
  return subtle.importKey("jwk", jwk, { name: "ECDH", namedCurve: "P-256" }, false, ["deriveBits"]);
}

test("тест-векторы ядра сходятся с WebCrypto", async () => {
  assert.ok(vectors.length >= 2);
  for (const v of vectors) {
    const A = hex(v.a_public);
    const B = hex(v.b_public);
    const commit = new Uint8Array(await subtle.digest("SHA-256", A));
    assert.equal(toHex(commit), v.commit);
    assert.equal(b64u(commit.slice(0, 16)), v.h);
    const macKey = await subtle.importKey("raw", hex(v.link_secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
    assert.equal(toHex(await subtle.sign("HMAC", macKey, B)), v.mac);
    const sasHash = new Uint8Array(await subtle.digest("SHA-256", cat(new TextEncoder().encode(`${v.prefix} sas`), A, B)));
    const sas = (new DataView(sasHash.buffer).getUint32(0) % 1_000_000).toString().padStart(6, "0");
    assert.equal(sas, v.sas);
    // Общий секрет с обеих сторон один и тот же.
    const pubB = await subtle.importKey("raw", B, { name: "ECDH", namedCurve: "P-256" }, false, []);
    const pubA = await subtle.importKey("raw", A, { name: "ECDH", namedCurve: "P-256" }, false, []);
    const shared = await subtle.deriveBits({ name: "ECDH", public: pubB }, await privateKey(v.a_secret, v.a_public), 256);
    const shared2 = await subtle.deriveBits({ name: "ECDH", public: pubA }, await privateKey(v.b_secret, v.b_public), 256);
    assert.equal(toHex(shared), toHex(shared2));
    const ikm = await subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
    const k = await subtle.deriveKey(
      { name: "HKDF", hash: "SHA-256", salt: cat(A, B), info: new TextEncoder().encode(`${v.prefix} key`) },
      ikm,
      { name: "AES-GCM", length: 256 },
      false,
      ["encrypt", "decrypt"],
    );
    const ct = await subtle.encrypt({ name: "AES-GCM", iv: hex(v.nonce) }, k, hex(v.payload));
    assert.equal(toHex(cat(hex(v.nonce), new Uint8Array(ct))), v.wrapped);
    const w = hex(v.wrapped);
    assert.equal(toHex(await subtle.decrypt({ name: "AES-GCM", iv: w.slice(0, 12) }, k, w.slice(12))), v.payload);
  }
});

test("ссылка для QR ведёт в Obsidian и несёт секрет", () => {
  const link = connectLink("https://sync.example.com:8443/base", "abcd1234efgh5678", "c2VjcmV0LXNlY3JldA", "szS-WgFatbGgO7YEQg1pWg");
  assert.ok(link.startsWith("obsidian://selfsync-connect?"));
  const q = new URLSearchParams(link.slice(link.indexOf("?") + 1));
  assert.equal(q.get("server"), "https://sync.example.com:8443/base");
  assert.equal(q.get("code"), "abcd1234efgh5678");
  assert.equal(q.get("m"), "c2VjcmV0LXNlY3JldA");
  assert.equal(q.get("h"), "szS-WgFatbGgO7YEQg1pWg");
});

test("цифры группами по три", () => {
  assert.equal(formatSas("042917"), "042 917");
  assert.equal(formatSas("12"), "12");
});

test("опрос: до done, на ошибке и по остановке", async () => {
  const run = async (answers: UiResult[], stopAfter = Infinity): Promise<{ last: UiResult | null; seen: number; slept: number }> => {
    let i = 0;
    let seen = 0;
    let slept = 0;
    const last = await pollHandoff({
      step: () => Promise.resolve(answers[Math.min(i++, answers.length - 1)]!),
      onResult: () => seen++,
      stopped: () => seen >= stopAfter,
      deadline: Number.MAX_SAFE_INTEGER,
      sleep: () => {
        slept++;
        return Promise.resolve();
      },
    });
    return { last, seen, slept };
  };
  const waiting: UiResult = { type: "handoff", step: "waiting", name: "" };
  const done: UiResult = { type: "handoff", step: "done", name: "phone" };
  const err: UiResult = { type: "error", code: "handoff_mac_mismatch", message: "x" };
  let r = await run([waiting, waiting, done]);
  assert.deepEqual([r.last, r.seen, r.slept], [done, 3, 2]);
  r = await run([waiting, err]);
  assert.deepEqual([r.last, r.seen], [err, 2]);
  r = await run([waiting], 3);
  assert.equal(r.seen, 3);
  // Срок вышел — ни одного шага.
  const none = await pollHandoff({
    step: () => Promise.resolve(done),
    onResult: () => assert.fail(),
    stopped: () => false,
    deadline: 0,
  });
  assert.equal(none, null);
});
