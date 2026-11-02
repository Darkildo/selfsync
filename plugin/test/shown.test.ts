// Открыта ли вкладка настроек: по строкам последней отрисовки, а не по containerEl.

import assert from "node:assert/strict";
import { test } from "node:test";

import { ShownRows } from "../src/ui/shown.ts";

test("вкладка открыта, пока хоть одна отрисованная строка в DOM", () => {
  const shown = new ShownRows();
  assert.equal(shown.any(), false, "ничего не рисовали");
  const a = { isConnected: true };
  const b = { isConnected: true };
  shown.add(a);
  shown.add(b);
  assert.equal(shown.any(), true);
  a.isConnected = false;
  assert.equal(shown.any(), true, "b ещё на экране");
  b.isConnected = false;
  assert.equal(shown.any(), false, "вкладку закрыли");
  b.isConnected = true;
  assert.equal(shown.any(), false, "убранные строки забыты: новая отрисовка добавит свои");
});
