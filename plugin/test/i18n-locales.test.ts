// Переводы из реестра: ключи и параметры как в en, выбор языка по коду Obsidian.

import assert from "node:assert/strict";
import { test } from "node:test";

import { formatBytes, language, resolveLanguage, setLanguage, t } from "../src/i18n.ts";
import { LOCALES } from "../src/i18n/locales.ts";
import { lintDict } from "../src/i18n/lint.ts";

test("переводы: ключи и параметры совпадают с en", () => {
  for (const [code, dict] of Object.entries(LOCALES)) {
    assert.equal(code, code.toLowerCase(), `${code}: код в реестре — в нижнем регистре`);
    assert.deepEqual(lintDict(dict).errors, [], code);
  }
});

test("ru: перевод полный", () => {
  const r = lintDict(LOCALES["ru"] ?? {});
  assert.equal(r.translated, r.total);
});

test("выбор языка: точный код, базовый язык, en", () => {
  assert.equal(resolveLanguage("ru"), "ru");
  assert.equal(resolveLanguage("RU_ru"), "ru");
  assert.equal(resolveLanguage("ru-RU"), "ru");
  assert.equal(resolveLanguage("en-GB"), "en");
  assert.equal(resolveLanguage("kab"), "en");
  assert.equal(resolveLanguage("constructor"), "en");
  assert.equal(resolveLanguage(""), "en");
});

test("нет строки в переводе — английская; письмо справа налево — параметры изолированы", () => {
  LOCALES["xx"] = { "status.idle": "X", "status.pending": "{n} X" };
  LOCALES["zh"] = { "status.idle": "Z" };
  LOCALES["ar"] = { "status.pending": "{n} A" };
  try {
    setLanguage("xx-YY");
    assert.equal(language(), "xx");
    assert.equal(t("status.idle"), "X");
    assert.equal(t("status.syncing"), "Syncing");
    assert.equal(t("status.pending", { n: 2 }), "2 X");
    setLanguage("zh-cn");
    assert.equal(t("status.idle"), "Z", "moment.locale() отдаёт zh-cn, у Obsidian это zh");
    setLanguage("zh-TW");
    assert.equal(t("status.idle"), "Z", "нет zh-tw — базовый zh");
    setLanguage("ar");
    assert.equal(t("status.pending", { n: 2 }), "⁨2⁩ A");
    assert.equal(formatBytes(2048), "2.0 KB");
  } finally {
    delete LOCALES["xx"];
    delete LOCALES["zh"];
    delete LOCALES["ar"];
    setLanguage("en");
  }
});
