// Бэкенд на vault.adapter Obsidian — для мобильных, где нет Node fs.
//
// adapter.rename не заменяет существующий файл, поэтому атомарная запись идёт по
// протоколу из контракта Action::Write: X.selfsync-tmp → X.commit.selfsync-tmp
// (записан целиком) → отодвинуть X в X.aside.selfsync-tmp → переименовать в X (для
// имени, которому не хватает места под суффикс, вместо X — укороченная база,
// `tempBase`). Прерванную на любом шаге запись ядро доведёт или повторит.
// readBinary читает файл целиком — отсюда лимит размера файла на мобильных (README)
// и кэш файла, читаемого по частям.

import type { Expect, FileMeta } from "../types.ts";
import {
  asideNames,
  DirNames,
  type FileBackend,
  joinPath,
  nameOf,
  parentOf,
  realName,
  satisfies,
  TEMP_SUFFIX,
  tempBase,
  type WriteOutcome,
} from "./backend.ts";

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

const COMMIT_SUFFIX = ".commit.selfsync-tmp";
/** Сколько stat одновременно при обходе: каждый — вызов через мост приложения. */
const LIST_CONCURRENCY = 12;

function buffer(data: Uint8Array): ArrayBuffer {
  return data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer;
}

/** Файл, который сейчас читается по частям. */
interface Cached {
  path: string;
  size: number;
  mtime: number;
  data: Uint8Array;
}

export class AdapterBackend implements FileBackend {
  /** readBinary читает файл целиком, а ядро читает большие файлы кусками по 4 МБ:
   *  без кэша файл в 200 МБ перечитывался бы целиком на каждый кусок. Держим один
   *  недочитанный файл; любая запись в бэкенд его сбрасывает. */
  private cached: Cached | null = null;
  /** Листинги каталогов для настоящего имени в stat. */
  private names = new DirNames();

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
      this.names.put(dir, [...l.files, ...l.folders].map(nameOf));
      for (const f of l.folders) {
        const path = joinPath(f);
        out.push({ path, size: 0, mtime: 0, dir: true });
        queue.push(path);
      }
      // stat по одному — последовательные вызовы через мост приложения: на
      // телефоне полный обход большого vault'а шёл бы минутами.
      const files = l.files.map((f) => joinPath(f));
      const stats = await mapLimit(files, LIST_CONCURRENCY, (path) => this.a.stat(path));
      files.forEach((path, i) => {
        const st = stats[i];
        if (st?.type === "file") out.push({ path, size: st.size, mtime: Math.trunc(st.mtime), dir: false });
      });
    }
    return out;
  }

  async stat(path: string): Promise<FileMeta | null> {
    const st = await this.a.stat(path);
    if (!st) return null;
    const parent = parentOf(path);
    const names = await this.names.get(parent, async () => {
      const l = await this.a.list(parent === "" ? "/" : parent);
      return [...l.files, ...l.folders].map(nameOf);
    });
    const real = joinPath(parent, realName(names, nameOf(path)) ?? nameOf(path));
    return st.type === "folder"
      ? { path: real, size: 0, mtime: 0, dir: true }
      : { path: real, size: st.size, mtime: Math.trunc(st.mtime), dir: false };
  }

  async read(path: string, offset: number, len?: number): Promise<Uint8Array | null> {
    return this.readPart(path, offset, len);
  }

  /** Часть файла; файл, дочитанный не до конца, остаётся в кэше до следующего куска. */
  private async readPart(path: string, offset: number, len?: number): Promise<Uint8Array | null> {
    const st = await this.a.stat(path);
    if (!st) return null;
    const c = this.cached;
    const hit = c !== null && c.path === path && c.size === st.size && c.mtime === st.mtime;
    const all = hit ? c.data : new Uint8Array(await this.a.readBinary(path));
    const start = Math.min(offset, all.length);
    const end = len === undefined ? all.length : Math.min(all.length, start + len);
    this.cached = end < all.length ? { path, size: st.size, mtime: st.mtime, data: all } : null;
    return all.slice(start, end);
  }

  private forget(): void {
    this.cached = null;
  }

  async writeAtomic(path: string, data: Uint8Array, expect: Expect): Promise<WriteOutcome> {
    this.forget();
    // Не выполнено уже сейчас — не писать зря (каждый вызов — через мост приложения).
    if (!satisfies(await this.stat(path), expect)) return { kind: "precondition" };
    await this.ensureDir(parentOf(path));
    // Длинное имя с суффиксом не поместилось бы в 255 байт: база укорачивается, а
    // ядро находит исходный путь по хэшу в укороченном имени (`tempname` в ядре).
    const tmp = tempBase(path) + TEMP_SUFFIX;
    const commit = tempBase(path) + COMMIT_SUFFIX;
    await this.a.writeBinary(tmp, buffer(data));
    await this.replace(tmp, commit);
    return this.place(commit, path, expect);
  }

  /**
   * Поставить готовый `src` (наш временный файл) на место `path`, если путь всё ещё
   * удовлетворяет `expect`. Заменяемый файл не стирается, а отодвигается
   * переименованием в `X.aside.selfsync-tmp`: так он «пойман» целиком, и условие
   * сверяется уже по нему. Изменился — возвращается на место, запись отменяется;
   * пока его отодвигали, на месте появился новый (пользователь сохранил) — тот
   * остаётся, adapter.rename его не заменит.
   */
  private async place(src: string, path: string, expect: Expect): Promise<WriteOutcome> {
    const refuse = async (): Promise<WriteOutcome> => {
      if (await this.a.exists(src)) await this.a.remove(src);
      return { kind: "precondition" };
    };
    let displaced: string | undefined;
    if (expect.kind !== "absent" && (await this.a.exists(path, true))) {
      const aside = await this.freeAside(path);
      await this.a.rename(path, aside);
      const before = await this.a.stat(aside);
      const same = before?.type === "file" && (expect.kind === "any" || (before.size === expect.size && Math.trunc(before.mtime) === expect.mtime));
      if (!same || (await this.a.exists(path))) {
        // Изменился до того, как его отодвинули, — вернуть на место. Место уже
        // занято новой версией — отодвинутый подберёт полный обход (ядро).
        if (!(await this.a.exists(path))) await this.a.rename(aside, path);
        else if (same && expect.kind === "stat") await this.a.remove(aside);
        return refuse();
      }
      try {
        await this.a.rename(src, path);
      } catch (e) {
        // Пока отодвигали, на месте сохранили новый файл: он остаётся.
        if (await this.a.exists(path)) {
          await this.a.remove(aside);
          return refuse();
        }
        throw e;
      }
      const after = await this.a.stat(aside);
      if (expect.kind === "stat" && after && (after.size !== before.size || after.mtime !== before.mtime)) displaced = aside;
      else await this.a.remove(aside);
    } else {
      if (expect.kind === "absent" && (await this.a.exists(path))) return refuse();
      try {
        await this.a.rename(src, path);
      } catch (e) {
        if (await this.a.exists(path)) return refuse();
        throw e;
      }
    }
    this.names.added(path);
    const meta = (await this.stat(path)) ?? { path, size: 0, mtime: 0, dir: false };
    return displaced === undefined ? { kind: "written", meta } : { kind: "written", meta, displaced };
  }

  /** Свободное имя, под которое отодвигается заменяемый файл. */
  private async freeAside(path: string): Promise<string> {
    for (const name of asideNames(path)) {
      if (!(await this.a.exists(name))) return name;
    }
    throw new Error(`${path}: нет свободного временного имени`);
  }

  /** Переименование с заменой существующего (два шага, не атомарно). */
  private async replace(from: string, to: string): Promise<void> {
    if (await this.a.exists(to, true)) await this.a.remove(to);
    await this.a.rename(from, to);
  }

  private async ensureDir(dir: string): Promise<void> {
    if (dir !== "" && !(await this.a.exists(dir))) {
      await this.a.mkdir(dir);
      this.names.added(dir);
    }
  }

  async rename(from: string, to: string): Promise<void> {
    this.forget();
    await this.ensureDir(parentOf(to));
    await this.a.rename(from, to);
    this.names.removed(from);
    this.names.added(to);
  }

  async mkdir(path: string): Promise<void> {
    await this.ensureDir(path);
  }

  async rmdir(path: string): Promise<boolean> {
    this.forget();
    const l = await this.a.list(path);
    if (l.files.length > 0 || l.folders.length > 0) return false;
    await this.a.rmdir(path, false);
    this.names.removed(path);
    return true;
  }

  async remove(path: string): Promise<void> {
    this.forget();
    await this.a.remove(path);
    this.names.removed(path);
  }

  async trash(path: string): Promise<void> {
    this.forget();
    this.names.removed(path);
    if (this.trashMode === "system" && (await this.a.trashSystem(path))) return;
    await this.a.trashLocal(path);
  }

  async tempWrite(name: string, offset: number, data: Uint8Array): Promise<void> {
    this.forget();
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
    return this.readPart(this.stored(`tmp/${name}`), offset, len);
  }

  async tempDelete(name: string): Promise<void> {
    this.forget();
    const f = this.stored(`tmp/${name}`);
    if (await this.a.exists(f)) await this.a.remove(f);
  }

  async tempCommit(name: string, path: string, expect: Expect): Promise<WriteOutcome> {
    this.forget();
    if (!satisfies(await this.stat(path), expect)) return { kind: "precondition" };
    await this.ensureDir(parentOf(path));
    const commit = tempBase(path) + COMMIT_SUFFIX;
    await this.replace(this.stored(`tmp/${name}`), commit);
    return this.place(commit, path, expect);
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
    this.forget();
    const f = this.stored(name);
    await this.ensureDir(parentOf(f));
    await this.a.writeBinary(f + TEMP_SUFFIX, buffer(data));
    await this.replace(f + TEMP_SUFFIX, f);
  }

  async storeDelete(name: string): Promise<void> {
    this.forget();
    const f = this.stored(name);
    if (await this.a.exists(f)) await this.a.remove(f);
  }
}

/** `f` по каждому элементу, не больше `limit` одновременно; результат — в порядке входа. */
export async function mapLimit<T, R>(items: T[], limit: number, f: (x: T) => Promise<R>): Promise<R[]> {
  const out = new Array<R>(items.length);
  let next = 0;
  const worker = async (): Promise<void> => {
    while (next < items.length) {
      const i = next++;
      out[i] = await f(items[i] as T);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out;
}
