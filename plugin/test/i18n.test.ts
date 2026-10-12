// Тексты интерфейса, которые собираются из состояния.

import assert from "node:assert/strict";
import { test } from "node:test";

import { noticeText, setLanguage, storageText } from "../src/i18n.ts";

test("хранилище на сервере: строка настроек и уведомление", () => {
  setLanguage("ru");
  assert.equal(storageText(undefined), null, "старый сервер — строки нет");
  assert.equal(storageText({ verifiedAt: 0, damaged: 0, serverProblem: false }), null);
  assert.match(storageText({ verifiedAt: 1, damaged: 0, serverProblem: false }) ?? "", /^Проблем нет; проверено /);
  const bad = storageText({ verifiedAt: 0, damaged: 2, serverProblem: true }) ?? "";
  assert.match(bad, /только админ/);
  assert.match(bad, /повреждено файлов: 2/);
  assert.match(bad, /ещё не проверялось$/);
  assert.match(noticeText({ kind: "storageDamaged", damaged: 3, serverProblem: false }) ?? "", /файлов: 3/);
  assert.match(noticeText({ kind: "storageDamaged", damaged: 0, serverProblem: true }) ?? "", /админ/);
  setLanguage("en");
});

test("место на сервере: строка настроек", async () => {
  const { quotaText } = await import("../src/i18n.ts");
  setLanguage("ru");
  assert.equal(quotaText(undefined), null, "старый сервер — строки нет");
  assert.equal(quotaText({ used: 0, soft: 0, hard: 0, held: 0 }), null);
  assert.equal(quotaText({ used: 2048, soft: 0, hard: 0, held: 0 }), "занято 2.0 КБ");
  const full = quotaText({ used: 2048, soft: 1024, hard: 4096, held: 3 }) ?? "";
  assert.match(full, /^занято 2.0 КБ из 4.0 КБ, предупреждение с 1.0 КБ\. Места нет: файлов ждут отправки — 3/);
  setLanguage("en");
});

test("проверка соединения: тексты шагов", async () => {
  const { diagText } = await import("../src/i18n.ts");
  setLanguage("ru");
  assert.equal(diagText({ step: "server", level: "ok", code: "ok", detail: "0.4.0" }), "отвечает, selfsync 0.4.0");
  assert.match(diagText({ step: "server", level: "fail", code: "tls", detail: "" }), /сертификат/);
  assert.match(diagText({ step: "body", level: "fail", code: "body_limit", detail: "HTTP 413" }), /client_max_body_size/);
  assert.match(diagText({ step: "longpoll", level: "warn", code: "no_longpoll", detail: "" }), /Сервер работает постоянно/);
  assert.equal(diagText({ step: "token", level: "ok", code: "read_only", detail: "" }), "принят, устройство только для чтения");
  assert.equal(diagText({ step: "proxy", level: "ok", code: "ok", detail: "" }), "в порядке");
  assert.equal(diagText({ step: "body", level: "warn", code: "new_code", detail: "x" }), "new_code x");
  setLanguage("en");
});
