// Примитивы ввода-вывода, на которых исполнитель строит действия ядра. Проверки
// условий (expect) и коды результатов — в executor.ts, здесь только операции.
//
// Пути — относительно корня vault'а, разделитель «/», как их видит ядро.

import type { FileMeta } from "../types.ts";

export interface FileBackend {
  /** Все файлы и папки vault'а. */
  list(): Promise<FileMeta[]>;
  /**
   * Метаданные пути с НАСТОЯЩИМ именем на диске в `path`: на регистронезависимой ФС
   * по имени «Note.md» может найтись «note.md» — ядро обязано это видеть.
   */
  stat(path: string): Promise<FileMeta | null>;
  /** Байты файла (`len` не задан — до конца); `null` — файла нет. */
  read(path: string, offset: number, len?: number): Promise<Uint8Array | null>;
  /** Атомарно записать файл целиком, создав родительские папки. */
  writeAtomic(path: string, data: Uint8Array): Promise<FileMeta>;
  /** Переименовать (назначение свободно — проверено), создав родительские папки. */
  rename(from: string, to: string): Promise<void>;
  mkdir(path: string): Promise<void>;
  /** Удалить пустую папку; `false` — не пуста. */
  rmdir(path: string): Promise<boolean>;
  /** В корзину (не стирать безвозвратно). */
  trash(path: string): Promise<void>;

  // Личное хранилище плагина: временные файлы скачивания, кэш, индекс.
  tempWrite(name: string, offset: number, data: Uint8Array): Promise<void>;
  tempRead(name: string, offset: number, len: number): Promise<Uint8Array | null>;
  tempDelete(name: string): Promise<void>;
  /** Атомарно перенести временный файл на место в vault'е. */
  tempCommit(name: string, path: string): Promise<FileMeta>;
  storeRead(name: string): Promise<Uint8Array | null>;
  storeWrite(name: string, data: Uint8Array): Promise<void>;
  storeDelete(name: string): Promise<void>;
}

export interface HttpResponse {
  status: number;
  headers: [string, string][];
  body: Uint8Array;
}

export interface HttpBackend {
  /** Бросает исключение, если ответа нет (сеть, таймаут). */
  request(url: string, method: string, headers: [string, string][], body: Uint8Array, timeoutMs: number): Promise<HttpResponse>;
}

/** Склеить части пути vault'а, убрав пустые сегменты. */
export function joinPath(...parts: string[]): string {
  return parts
    .flatMap((p) => p.split("/"))
    .filter((p) => p !== "" && p !== ".")
    .join("/");
}

export function parentOf(path: string): string {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i);
}

export function nameOf(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/**
 * Настоящее имя записи каталога для запрошенного: точное совпадение, иначе то, что
 * совпадает без учёта регистра и нормализации Unicode (так ищет ФС macOS/Windows).
 */
export function realName(entries: string[], wanted: string): string | undefined {
  if (entries.includes(wanted)) return wanted;
  const key = wanted.normalize("NFC").toLowerCase();
  return entries.find((e) => e.normalize("NFC").toLowerCase() === key);
}
