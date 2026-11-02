// Реестр переводов: код языка → словарь. en — эталон (en.ts), в реестр не входит.
//
// Новый язык (подробно — TRANSLATING.md):
//   1. файл <код>.ts рядом, формат — как в TRANSLATING.md;
//   2. импорт ниже, по алфавиту кодов: `import de from "./de.ts";`
//   3. строка в LOCALES, тоже по алфавиту: `de,` или `"pt-br": ptBr,`.
// Код — как в списке языков Obsidian (LANGUAGES.md), в нижнем регистре: zh, zh-tw, pt-br.

import type { Dict } from "./en.ts";
import ru from "./ru.ts";

export const LOCALES: Record<string, Dict> = {
  ru,
};
