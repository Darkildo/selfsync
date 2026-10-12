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
