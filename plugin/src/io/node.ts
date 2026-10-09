// Бэкенд на Node fs: десктопный Obsidian (Electron) и тесты. Даёт то, чего нет
// у vault.adapter: атомарную замену файла (rename поверх существующего) и чтение
// больших файлов частями.
//
// Модуль fs передаётся снаружи: на мобильных `require("fs")` нет, и main.js не
// должен тянуть его при загрузке.

import type * as FsPromises from "node:fs/promises";

import type { FileMeta } from "../types.ts";
import { type FileBackend, joinPath, nameOf, parentOf, realName } from "./backend.ts";

const TEMP_SUFFIX = ".notesync-tmp";

export interface NodeBackendOptions {
  /** ФС различает регистр (Linux): настоящее имя можно не искать в каталоге. */
  caseSensitive?: boolean;
  /** Корзина Obsidian; по умолчанию — каталог `trash` в хранилище плагина. */
  trash?: (path: string) => Promise<void>;
}

export class NodeBackend implements FileBackend {
  constructor(
    /** Абсолютный путь к vault'у. */
    private root: string,
    /** Абсолютный путь к хранилищу плагина (индекс, кэш, временные файлы). */
    private store: string,
    private fsp: typeof FsPromises,
    private opts: NodeBackendOptions = {},
  ) {}

  private abs(p: string): string {
    return p ? `${this.root}/${p}` : this.root;
  }

  private stored(name: string): string {
    return `${this.store}/${name}`;
  }

  async list(): Promise<FileMeta[]> {
    const out: FileMeta[] = [];
    const walk = async (rel: string): Promise<void> => {
      const entries = await this.fsp.readdir(this.abs(rel), { withFileTypes: true });
      await Promise.all(
        entries.map(async (e) => {
          const path = joinPath(rel, e.name);
          if (e.isDirectory()) {
            out.push({ path, size: 0, mtime: 0, dir: true });
            await walk(path);
          } else if (e.isFile()) {
            const st = await this.fsp.lstat(this.abs(path)).catch(() => null);
            if (st) out.push({ path, size: st.size, mtime: Math.trunc(st.mtimeMs), dir: false });
          }
          // Символические ссылки не синхронизируются.
        }),
      );
    };
    await walk("");
    return out;
  }

  async stat(path: string): Promise<FileMeta | null> {
    const st = await this.fsp.lstat(this.abs(path)).catch(missing);
    if (!st) return null;
    let real = path;
    if (!this.opts.caseSensitive && path !== "") {
      const parent = parentOf(path);
      const entries = await this.fsp.readdir(this.abs(parent)).catch(() => [] as string[]);
      real = joinPath(parent, realName(entries, nameOf(path)) ?? nameOf(path));
    }
    return st.isDirectory()
      ? { path: real, size: 0, mtime: 0, dir: true }
      : { path: real, size: st.size, mtime: Math.trunc(st.mtimeMs), dir: false };
  }

  async read(path: string, offset: number, len?: number): Promise<Uint8Array | null> {
    return readRange(this.fsp, this.abs(path), offset, len);
  }

  async writeAtomic(path: string, data: Uint8Array): Promise<FileMeta> {
    await this.fsp.mkdir(this.abs(parentOf(path)), { recursive: true });
    await writeDurable(this.fsp, this.abs(path) + TEMP_SUFFIX, data);
    await this.fsp.rename(this.abs(path) + TEMP_SUFFIX, this.abs(path));
    return (await this.stat(path)) ?? { path, size: data.length, mtime: 0, dir: false };
  }

  async rename(from: string, to: string): Promise<void> {
    await this.fsp.mkdir(this.abs(parentOf(to)), { recursive: true });
    await this.fsp.rename(this.abs(from), this.abs(to));
  }

  async mkdir(path: string): Promise<void> {
    await this.fsp.mkdir(this.abs(path), { recursive: true });
  }

  async rmdir(path: string): Promise<boolean> {
    try {
      await this.fsp.rmdir(this.abs(path));
      return true;
    } catch (e) {
      const code = (e as NodeJS.ErrnoException).code;
      if (code === "ENOENT") return true;
      if (code === "ENOTEMPTY" || code === "EEXIST") return false;
      throw e;
    }
  }

  async trash(path: string): Promise<void> {
    if (this.opts.trash) return this.opts.trash(path);
    const dir = this.stored("trash");
    await this.fsp.mkdir(dir, { recursive: true });
    await this.fsp.rename(this.abs(path), `${dir}/${Date.now()}-${nameOf(path)}`);
  }

  async tempWrite(name: string, offset: number, data: Uint8Array): Promise<void> {
    const f = this.stored(`tmp/${name}`);
    if (offset === 0) {
      await this.fsp.mkdir(this.stored("tmp"), { recursive: true });
      await writeDurable(this.fsp, f, data);
      return;
    }
    const fh = await this.fsp.open(f, "r+");
    try {
      const { size } = await fh.stat();
      if (size < offset) throw new Error(`разрыв во временном файле ${name}: ${size} < ${offset}`);
      await fh.truncate(offset);
      await fh.write(data, 0, data.length, offset);
      await fh.sync();
    } finally {
      await fh.close();
    }
  }

  async tempRead(name: string, offset: number, len: number): Promise<Uint8Array | null> {
    return readRange(this.fsp, this.stored(`tmp/${name}`), offset, len);
  }

  async tempDelete(name: string): Promise<void> {
    await this.fsp.rm(this.stored(`tmp/${name}`), { force: true });
  }

  async tempCommit(name: string, path: string): Promise<FileMeta> {
    await this.fsp.mkdir(this.abs(parentOf(path)), { recursive: true });
    await this.fsp.rename(this.stored(`tmp/${name}`), this.abs(path));
    return (await this.stat(path)) ?? { path, size: 0, mtime: 0, dir: false };
  }

  async storeRead(name: string): Promise<Uint8Array | null> {
    return this.fsp.readFile(this.stored(name)).then((b) => new Uint8Array(b), missing);
  }

  async storeWrite(name: string, data: Uint8Array): Promise<void> {
    const f = this.stored(name);
    await this.fsp.mkdir(parentOf(f), { recursive: true });
    await writeDurable(this.fsp, f + TEMP_SUFFIX, data);
    await this.fsp.rename(f + TEMP_SUFFIX, f);
  }

  async storeDelete(name: string): Promise<void> {
    await this.fsp.rm(this.stored(name), { force: true });
  }
}

/** Файла (или пути к нему) нет — `null`, остальные ошибки пробрасываются. */
function missing(e: unknown): null {
  const code = (e as NodeJS.ErrnoException).code;
  if (code === "ENOENT" || code === "ENOTDIR") return null;
  throw e;
}

/** Запись с fsync: после rename поверх старого файла не останется пустышки. */
async function writeDurable(fsp: typeof FsPromises, file: string, data: Uint8Array): Promise<void> {
  const fh = await fsp.open(file, "w");
  try {
    await fh.write(data, 0, data.length, 0);
    await fh.sync();
  } finally {
    await fh.close();
  }
}

async function readRange(fsp: typeof FsPromises, file: string, offset: number, len?: number): Promise<Uint8Array | null> {
  const fh = await fsp.open(file, "r").catch(missing);
  if (!fh) return null;
  try {
    const { size } = await fh.stat();
    const start = Math.min(offset, size);
    const end = len === undefined ? size : Math.min(size, start + len);
    const buf = new Uint8Array(end - start);
    let got = 0;
    while (got < buf.length) {
      const { bytesRead } = await fh.read(buf, got, buf.length - got, start + got);
      if (bytesRead === 0) break;
      got += bytesRead;
    }
    return got === buf.length ? buf : buf.subarray(0, got);
  } finally {
    await fh.close();
  }
}
