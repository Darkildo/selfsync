#!/usr/bin/env node
// Тест-векторы передачи ключа по коду (spec/vectors/handoff.json). Считаются
// криптографией Node (OpenSSL), а не ядром: ядро и плагины сверяются с независимой
// реализацией. Входы фиксированы, поэтому повторный запуск даёт тот же файл.
//
//   node scripts/handoff-vectors.mjs > spec/vectors/handoff.json

import { createCipheriv, createECDH, createHash, createHmac, hkdfSync } from "node:crypto";

const hex = (b) => Buffer.from(b).toString("hex");
const fill = (n, start) => Buffer.from(Array.from({ length: n }, (_, i) => (start + i * 7) & 0xff));

function one(prefix, aSecret, bSecret, linkSecret, nonce, payload) {
  const a = createECDH("prime256v1");
  a.setPrivateKey(aSecret);
  const b = createECDH("prime256v1");
  b.setPrivateKey(bSecret);
  const A = a.getPublicKey(null, "uncompressed");
  const B = b.getPublicKey(null, "uncompressed");
  const sasHash = createHash("sha256").update(`${prefix} sas`).update(A).update(B).digest();
  const sas = String(sasHash.readUInt32BE(0) % 1_000_000).padStart(6, "0");
  const shared = a.computeSecret(B);
  const k = Buffer.from(hkdfSync("sha256", shared, Buffer.concat([A, B]), `${prefix} key`, 32));
  const c = createCipheriv("aes-256-gcm", k, nonce);
  const wrapped = Buffer.concat([nonce, c.update(payload), c.final(), c.getAuthTag()]);
  return {
    prefix,
    a_secret: hex(aSecret),
    b_secret: hex(bSecret),
    a_public: hex(A),
    b_public: hex(B),
    commit: createHash("sha256").update(A).digest("hex"),
    link_secret: hex(linkSecret),
    mac: createHmac("sha256", linkSecret).update(B).digest("hex"),
    sas,
    nonce: hex(nonce),
    payload: hex(payload),
    wrapped: hex(wrapped),
  };
}

const cases = [
  one("selfsync/join-handoff/v1", fill(32, 1), fill(32, 101), fill(16, 201), fill(12, 31), fill(32, 61)),
  one("selfsync-cloud/handoff/v1", fill(32, 2), fill(32, 102), fill(16, 202), fill(12, 32), fill(32, 62)),
  one("selfsync/join-handoff/v1", fill(32, 3), fill(32, 103), fill(16, 203), fill(12, 33), fill(32, 63)),
];

const doc = {
  about:
    "Передача ключа по коду: P-256 (несжатые точки SEC1), commit = sha256(A), mac = HMAC-SHA256(link_secret, B), " +
    "sas = sha256(prefix + ' sas' ‖ A ‖ B)[0..4] BE mod 10^6, k = HKDF-SHA256(ECDH x, salt = A ‖ B, info = prefix + ' key'), " +
    "wrapped = nonce ‖ AES-256-GCM(k, payload) без AAD. Байты — hex. Генератор: scripts/handoff-vectors.mjs",
  cases,
};
process.stdout.write(`${JSON.stringify(doc, null, 2)}\n`);
