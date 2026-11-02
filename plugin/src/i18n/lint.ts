// Проверка словаря перевода против эталона (по умолчанию en): тест, npm run i18n:check
// и selfsync-cloud для своих словарей. Без Node API — лежит в src/, чтобы копировалось ядро.

import { en } from "./en.ts";

export interface LintResult {
  errors: string[];
  warnings: string[];
  translated: number;
  total: number;
}

function params(s: string): string {
  return [...new Set([...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]))].sort().join(",");
}

/** Ошибки ломают сборку; предупреждения — только подсказка переводчику. */
export function lintDict(dict: Record<string, unknown>, ref: Record<string, string> = en): LintResult {
  const errors: string[] = [];
  const warnings: string[] = [];
  let translated = 0;
  for (const [key, value] of Object.entries(dict)) {
    if (!(key in ref)) {
      errors.push(`${key}: такого ключа нет в en.ts`);
      continue;
    }
    if (typeof value !== "string" || value.trim() === "") {
      errors.push(`${key}: пустая строка`);
      continue;
    }
    const want = params(ref[key] ?? "");
    const got = params(value);
    if (want !== got) errors.push(`${key}: параметры {${got}} вместо {${want}}`);
    if (value === ref[key] && value.length > 25) warnings.push(`${key}: совпадает с английским`);
    translated++;
  }
  return { errors, warnings, translated, total: Object.keys(ref).length };
}
