// Журнал синхронизации: последние строки в памяти и в файле каталога плагина. Без
// файла журнал после перезапуска Obsidian пуст, и разбор «что сломалось» начинался
// бы с нуля. Файл переписывается целиком не чаще раза в несколько секунд и только
// если появились новые строки.

export interface LogStore {
  read(): Promise<string | null>;
  write(text: string): Promise<void>;
}

/** Имя файла журнала в каталоге плагина (синк настроек его не переносит). */
export const LOG_FILE = "sync.log";
const MAX_BYTES = 100 * 1024;
const FLUSH_MS = 5000;

const encoder = new TextEncoder();

function size(line: string): number {
  return encoder.encode(line).length + 1;
}

export class SyncLog {
  private lines: string[] = [];
  private bytes = 0;
  private dirty = false;
  private timer: number | undefined;
  private loaded: Promise<void> | undefined;

  constructor(
    private store: LogStore | null,
    private maxBytes = MAX_BYTES,
    private flushMs = FLUSH_MS,
  ) {}

  /** Подхватить журнал прошлых запусков — он встаёт перед новыми строками. */
  load(): Promise<void> {
    this.loaded ??= (async () => {
      const old = await this.store?.read().catch(() => null);
      if (!old) return;
      this.lines = [...old.split("\n").filter((l) => l !== ""), ...this.lines];
      this.bytes = this.lines.reduce((n, l) => n + size(l), 0);
      this.trim();
    })();
    return this.loaded;
  }

  add(line: string): void {
    this.lines.push(line);
    this.bytes += size(line);
    this.trim();
    this.dirty = true;
    if (this.store && this.timer === undefined) {
      this.timer = window.setTimeout(() => {
        this.timer = undefined;
        void this.flush();
      }, this.flushMs);
    }
  }

  /** Последние `n` строк (все — без `n`). */
  tail(n?: number): string[] {
    return n === undefined ? [...this.lines] : this.lines.slice(-n);
  }

  /** Записать накопленное сейчас (выгрузка плагина). */
  async flush(): Promise<void> {
    if (this.timer !== undefined) {
      window.clearTimeout(this.timer);
      this.timer = undefined;
    }
    if (!this.dirty || !this.store) return;
    // Иначе новые строки затёрли бы ещё не прочитанный журнал прошлого запуска.
    await this.loaded;
    this.dirty = false;
    await this.store.write(`${this.lines.join("\n")}\n`).catch(() => {
      // Журнал — подсказка, а не данные: ошибка записи не должна мешать синку.
    });
  }

  private trim(): void {
    while (this.bytes > this.maxBytes && this.lines.length > 1) {
      this.bytes -= size(this.lines.shift() ?? "");
    }
  }
}
