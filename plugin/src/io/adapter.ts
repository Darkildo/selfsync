// Бэкенд на vault.adapter Obsidian — для мобильных, где нет Node fs.
//
// adapter.rename не заменяет существующий файл, поэтому атомарная запись идёт по
// протоколу из контракта Action::Write: X.selfsync-tmp → X.commit.selfsync-tmp
// (записан целиком) → удалить X → переименовать в X. Прерванную на любом шаге
// запись ядро доведёт или повторит. readBinary читает файл целиком — отсюда
// лимит размера файла на мобильных (README).

import type { FileMeta } from "../types.ts";
import { type FileBackend, joinPath, nameOf, parentOf, realName } from "./backend.ts";

/** То подмножество DataAdapter, которым пользуется бэкенд (удобно подменять в тестах). */
export interface AdapterLike {
  exists(path: string, sensitive?: boolean): Promise<boolean>;
  stat(path: string): Promise<{ type: "file" | "folder"; size: number; mtime: number } | null>;
  list(path: string): Promise<{ files: string[]; folders: string[] }>;
  readBinary(path: string): Promise<ArrayBuffer>;
  writeBinary(path: string, data: ArrayBuffer): Promise<void>;
  /** Есть только с Obsidian 1.12.3; без него докачка переписывает временный файл целиком. */
  appendBinary?(path: string, data: ArrayBuffer): Promise<void>;
  mkdir(path: string): Promise<void>;
  rmdir(path: string, recursive: boolean): Promise<void>;
  remove(path: string): Promise<void>;
  rename(from: string, to: string): Promise<void>;
  trashSystem(path: string): Promise<boolean>;
  trashLocal(path: string): Promise<void>;
}

const TEMP_SUFFIX = ".selfsync-tmp";
const COMMIT_SUFFIX = ".commit.selfsync-tmp";

function buffer(data: Uint8Array): ArrayBuffer {
  return data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer;
}

export class AdapterBackend implements FileBackend {
  constructor(
    private a: AdapterLike,
    /** Каталог плагина относительно vault'а (`.obsidian/plugins/selfsync`). */
    private pluginDir: string,
    /** Куда убирать удалённое: системная корзина или `.trash` в vault'е. */
    private trashMode: "system" | "local" = "system",
  ) {}

  private stored(name: string): string {
    return joinPath(this.pluginDir, name);
  }

  async list(): Promise<FileMeta[]> {
    const out: FileMeta[] = [];
    const queue = [""];
    while (queue.length > 0) {
      const dir = queue.shift() ?? "";
      const l = await this.a.list(dir === "" ? "/" : dir);
      for (const f of l.folders) {
        const path = joinPath(f);
        out.push({ path, size: 0, mtime: 0, dir: true });
        queue.push(path);
      }
      for (const f of l.files) {
        const path = joinPath(f);
        const st = await this.a.stat(path);
        if (st?.type === "file") out.push({ path, size: st.size, mtime: Math.trunc(st.mtime), dir: false });
      }
    }
    return out;
  }

  async stat(path: string): Promise<FileMeta | null> {
    const st = await this.a.stat(path);
    if (!st) return null;
    const parent = parentOf(path);
    const l = await this.a.list(parent === "" ? "/" : parent);
    const names = [...l.files, ...l.folders].map(nameOf);
    const real = joinPath(parent, realName(names, nameOf(path)) ?? nameOf(path));
    return st.type === "folder"
      ? { path: real, size: 0, mtime: 0, dir: true }
      : { path: real, size: st.size, mtime: Math.trunc(st.mtime), dir: false };
  }

  async read(path: string, offset: number, len?: number): Promise<Uint8Array | null> {
    if (!(await this.a.exists(path))) return null;
    const all = new Uint8Array(await this.a.readBinary(path));
    const start = Math.min(offset, all.length);
    return all.slice(start, len === undefined ? all.length : Math.min(all.length, start + len));
  }

  async writeAtomic(path: string, data: Uint8Array): Promise<FileMeta> {
    await this.ensureDir(parentOf(path));
    const tmp = path + TEMP_SUFFIX;
    const commit = path + COMMIT_SUFFIX;
    await this.a.writeBinary(tmp, buffer(data));
    await this.replace(tmp, commit);
    await this.replace(commit, path);
    return (await this.stat(path)) ?? { path, size: data.length, mtime: 0, dir: false };
  }

  /** Переименование с заменой существующего (два шага, не атомарно). */
  private async replace(from: string, to: string): Promise<void> {
    if (await this.a.exists(to, true)) await this.a.remove(to);
    await this.a.rename(from, to);
  }

  private async ensureDir(dir: string): Promise<void> {
    if (dir !== "" && !(await this.a.exists(dir))) await this.a.mkdir(dir);
  }

  async rename(from: string, to: string): Promise<void> {
    await this.ensureDir(parentOf(to));
    await this.a.rename(from, to);
  }

  async mkdir(path: string): Promise<void> {
    await this.ensureDir(path);
  }

  async rmdir(path: string): Promise<boolean> {
    const l = await this.a.list(path);
    if (l.files.length > 0 || l.folders.length > 0) return false;
    await this.a.rmdir(path, false);
    return true;
  }

  async trash(path: string): Promise<void> {
    if (this.trashMode === "system" && (await this.a.trashSystem(path))) return;
    await this.a.trashLocal(path);
  }

  async tempWrite(name: string, offset: number, data: Uint8Array): Promise<void> {
    const f = this.stored(`tmp/${name}`);
    if (offset === 0) {
      await this.ensureDir(this.stored("tmp"));
      await this.a.writeBinary(f, buffer(data));
      return;
    }
    const size = (await this.a.stat(f))?.size ?? 0;
    if (size === offset && this.a.appendBinary) {
      await this.a.appendBinary(f, buffer(data));
      return;
    }
    if (size < offset) throw new Error(`разрыв во временном файле ${name}: ${size} < ${offset}`);
    // Хвост после смещения (повтор части после сбоя) или старый Obsidian без
    // appendBinary — переписать файл целиком.
    const head = new Uint8Array(await this.a.readBinary(f)).subarray(0, offset);
    const all = new Uint8Array(offset + data.length);
    all.set(head, 0);
    all.set(data, offset);
    await this.a.writeBinary(f, buffer(all));
  }

  async tempRead(name: string, offset: number, len: number): Promise<Uint8Array | null> {
    const f = this.stored(`tmp/${name}`);
    if (!(await this.a.exists(f))) return null;
    const all = new Uint8Array(await this.a.readBinary(f));
    const start = Math.min(offset, all.length);
    return all.slice(start, Math.min(all.length, start + len));
  }

  async tempDelete(name: string): Promise<void> {
    const f = this.stored(`tmp/${name}`);
    if (await this.a.exists(f)) await this.a.remove(f);
  }

  async tempCommit(name: string, path: string): Promise<FileMeta> {
    await this.ensureDir(parentOf(path));
    const commit = path + COMMIT_SUFFIX;
    await this.replace(this.stored(`tmp/${name}`), commit);
    await this.replace(commit, path);
    return (await this.stat(path)) ?? { path, size: 0, mtime: 0, dir: false };
  }

  async storeRead(name: string): Promise<Uint8Array | null> {
    const f = this.stored(name);
    if (await this.a.exists(f)) return new Uint8Array(await this.a.readBinary(f));
    // Убиты между удалением старого и переименованием нового: временный файл к этому
    // моменту записан целиком (старый удаляется только после записи).
    if (await this.a.exists(f + TEMP_SUFFIX)) return new Uint8Array(await this.a.readBinary(f + TEMP_SUFFIX));
    return null;
  }

  async storeWrite(name: string, data: Uint8Array): Promise<void> {
    const f = this.stored(name);
    await this.ensureDir(parentOf(f));
    await this.a.writeBinary(f + TEMP_SUFFIX, buffer(data));
    await this.replace(f + TEMP_SUFFIX, f);
  }

  async storeDelete(name: string): Promise<void> {
    const f = this.stored(name);
    if (await this.a.exists(f)) await this.a.remove(f);
  }
}
