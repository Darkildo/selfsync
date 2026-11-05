// Бэкенд на Node fs: десктопный Obsidian (Electron) и тесты. Даёт то, чего нет
// у vault.adapter: атомарную замену файла (rename поверх существующего) и чтение
// больших файлов частями.
//
// Модуль fs передаётся снаружи: на мобильных `require("fs")` нет, и main.js не
// должен тянуть его при загрузке.

type FsPromises = typeof import("node:fs/promises");

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

type Stats = import("node:fs").Stats;

export interface NodeBackendOptions {
  /** ФС различает регистр (Linux): настоящее имя можно не искать в каталоге. */
  caseSensitive?: boolean;
  /** Корзина Obsidian; по умолчанию — каталог `trash` в хранилище плагина. */
  trash?: (path: string) => Promise<void>;
}

export class NodeBackend implements FileBackend {
  /** Листинги каталогов для настоящего имени (регистронезависимые ФС). */
  private names = new DirNames();
  /** Каталоги vault'а, которые недавно были обычными папками (не ссылками). */
  private realDirs = new RealDirs();

  constructor(
    /** Абсолютный путь к vault'у. */
    private root: string,
    /** Абсолютный путь к хранилищу плагина (индекс, кэш, временные файлы). */
    private store: string,
    private fsp: FsPromises,
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
      this.names.put(rel, entries.map((e) => e.name));
      await Promise.all(
        entries.map(async (e) => {
          const path = joinPath(rel, e.name);
          if (e.isDirectory()) {
            out.push({ path, size: 0, mtime: 0, dir: true });
            await walk(path);
          } else if (e.isFile()) {
            // Пропасть из листинга может только исчезнувший файл: любая другая ошибка
            // (нет прав, EIO) обрывает обход, иначе ядро сочтёт файл удалённым.
            const st = await this.fsp.lstat(this.abs(path)).catch(missing);
            if (st) out.push(fileMeta(path, st));
          } else if (e.isSymbolicLink()) {
            // Ссылка (на папку или файл) может вести за пределы vault'а: не
            // синхронизируется, ядро исключает её вместе со всем под ней.
            out.push({ path, size: 0, mtime: 0, dir: false, link: true });
          }
        }),
      );
    };
    this.realDirs.clear();
    await walk("");
    for (const m of out) if (m.dir) this.realDirs.mark(m.path);
    return out;
  }

  /**
   * Первая символическая ссылка на пути — сама запись или папка выше (корень vault'а
   * не в счёт). Путь через ссылку уходит за пределы vault'а: такие пути не читаются
   * и не меняются. Проверяется по `lstat` каждой папки на пути; недавно проверенные
   * обычные папки помнятся (`RealDirs`).
   */
  private async linkOn(path: string): Promise<string | null> {
    let cur = "";
    for (const part of path.split("/")) {
      if (part === "") continue;
      cur = cur ? `${cur}/${part}` : part;
      if (this.realDirs.known(cur)) continue;
      const st = await this.fsp.lstat(this.abs(cur)).catch(missing);
      // Дальше ничего нет — и ссылок тоже.
      if (!st) return null;
      if (st.isSymbolicLink()) return cur;
      if (!st.isDirectory()) return null;
      this.realDirs.mark(cur);
    }
    return null;
  }

  /** Изменять можно только пути без символических ссылок. */
  private async inside(...paths: string[]): Promise<void> {
    for (const path of paths) {
      const link = await this.linkOn(path);
      if (link !== null) throw new Error(`${path}: путь ведёт через символическую ссылку ${link} — вне vault'а не пишем`);
    }
  }

  async stat(path: string): Promise<FileMeta | null> {
    if ((await this.linkOn(path)) !== null) return { path, size: 0, mtime: 0, dir: false, link: true };
    const st = await this.fsp.lstat(this.abs(path)).catch(missing);
    if (!st) return null;
    let real = path;
    if (!this.opts.caseSensitive && path !== "") {
      const parent = parentOf(path);
      const entries = await this.names.get(parent, async () => (await this.fsp.readdir(this.abs(parent)).catch(missing)) ?? []);
      real = joinPath(parent, realName(entries, nameOf(path)) ?? nameOf(path));
    }
    return st.isDirectory() ? { path: real, size: 0, mtime: 0, dir: true } : fileMeta(real, st);
  }

  async read(path: string, offset: number, len?: number): Promise<Uint8Array | null> {
    // Через ссылку — чужой файл, не из vault'а: его здесь нет.
    if ((await this.linkOn(path)) !== null) return null;
    return readRange(this.fsp, this.abs(path), offset, len);
  }

  async writeAtomic(path: string, data: Uint8Array, expect: Expect): Promise<WriteOutcome> {
    await this.inside(path);
    // Не выполнено уже сейчас — не писать зря.
    if (!satisfies(await this.stat(path), expect)) return { kind: "precondition" };
    await this.fsp.mkdir(this.abs(parentOf(path)), { recursive: true });
    // Рядом с файлом: rename в пределах каталога атомарен. Длинное имя (до 255 байт)
    // с суффиксом не поместилось бы — тогда база временного имени укорочена.
    const tmp = this.abs(tempBase(path)) + TEMP_SUFFIX;
    await writeDurable(this.fsp, tmp, data);
    return this.place(tmp, path, expect, true);
  }

  /**
   * Поставить готовый файл `src` на место `path`, если путь всё ещё удовлетворяет
   * `expect`. Проверки и замены здесь — в несколько вызовов (атомарного «заменить,
   * если не изменился» у ФС нет), поэтому заменяемый файл не стирается: на него
   * ставится жёсткая ссылка `X.aside.selfsync-tmp` (сам X при этом не пропадает ни на
   * миг). По ней условие сверяется уже после того, как файл «пойман», и после замены
   * видно, не дописали ли в него за это время — тогда это вытесненная правка.
   * `own` — `src` наш и при отказе удаляется (временный файл скачивания — нет).
   */
  private async place(src: string, path: string, expect: Expect, own: boolean): Promise<WriteOutcome> {
    const dst = this.abs(path);
    const refuse = async (): Promise<WriteOutcome> => {
      if (own) await this.fsp.rm(src, { force: true });
      return { kind: "precondition" };
    };
    let displaced: string | undefined;
    if (expect.kind === "absent") {
      // link не заменяет существующий файл: «создать, только если имени нет».
      try {
        await this.fsp.link(src, dst);
        await this.fsp.unlink(src);
      } catch (e) {
        if (code(e) === "EEXIST") return refuse();
        if (!noLinks(e)) throw e;
        if (await this.fsp.lstat(dst).catch(missing)) return refuse();
        await this.fsp.rename(src, dst);
      }
    } else if (expect.kind === "stat") {
      const aside = await this.freeAside(path);
      const asideAbs = this.abs(aside);
      let linked = true;
      try {
        await this.fsp.link(dst, asideAbs);
      } catch (e) {
        if (code(e) === "ENOENT") return refuse();
        if (!noLinks(e)) throw e;
        linked = false;
        // Без жёстких ссылок (FAT, часть сетевых ФС) прежний файл отодвигается
        // переименованием — на миг его нет на месте.
        try {
          await this.fsp.rename(dst, asideAbs);
        } catch (e2) {
          if (code(e2) === "ENOENT") return refuse();
          throw e2;
        }
      }
      const before = await this.fsp.lstat(asideAbs, { bigint: true });
      if (Number(before.size) !== expect.size || Number(before.mtimeMs) !== expect.mtime || !before.isFile()) {
        // Изменился ещё до того, как его поймали: остаётся на месте, запись — нет.
        if (linked) await this.fsp.unlink(asideAbs);
        else if (!(await this.fsp.lstat(dst).catch(missing))) await this.fsp.rename(asideAbs, dst);
        // Иначе на месте уже новый файл, а этот подберёт полный обход (ядро).
        return refuse();
      }
      if (!linked && (await this.fsp.lstat(dst).catch(missing))) {
        // Пока отодвигали, на месте сохранили новый файл: он остаётся, а отодвинутый
        // — известная версия, его не жалко.
        await this.fsp.rm(asideAbs, { force: true });
        return refuse();
      }
      await this.fsp.rename(src, dst);
      // ctime здесь не годится: его меняет и сама замена (у отодвинутого файла
      // становится меньше имён). mtime — с полной точностью ФС.
      const after = await this.fsp.lstat(asideAbs, { bigint: true }).catch(missing);
      if (after && (after.size !== before.size || after.mtimeNs !== before.mtimeNs)) {
        displaced = aside;
      } else {
        await this.fsp.rm(asideAbs, { force: true });
      }
    } else {
      await this.fsp.rename(src, dst);
    }
    this.names.added(path);
    const meta = (await this.stat(path)) ?? { path, size: 0, mtime: 0, dir: false };
    return displaced === undefined ? { kind: "written", meta } : { kind: "written", meta, displaced };
  }

  /** Свободное имя, под которое отодвигается заменяемый файл. */
  private async freeAside(path: string): Promise<string> {
    for (const name of asideNames(path)) {
      if (!(await this.fsp.lstat(this.abs(name)).catch(missing))) return name;
    }
    throw new Error(`${path}: нет свободного временного имени`);
  }

  async rename(from: string, to: string): Promise<void> {
    await this.inside(from, to);
    await this.fsp.mkdir(this.abs(parentOf(to)), { recursive: true });
    await this.fsp.rename(this.abs(from), this.abs(to));
    this.names.removed(from);
    this.realDirs.removed(from);
    this.names.added(to);
  }

  async mkdir(path: string): Promise<void> {
    await this.inside(path);
    await this.fsp.mkdir(this.abs(path), { recursive: true });
    this.names.added(path);
  }

  async rmdir(path: string): Promise<boolean> {
    await this.inside(path);
    try {
      await this.fsp.rmdir(this.abs(path));
      this.names.removed(path);
      this.realDirs.removed(path);
      return true;
    } catch (e) {
      const code = (e as { code?: string }).code;
      if (code === "ENOENT") {
        this.names.removed(path);
        this.realDirs.removed(path);
        return true;
      }
      if (code === "ENOTEMPTY" || code === "EEXIST") return false;
      throw e;
    }
  }

  async remove(path: string): Promise<void> {
    await this.inside(path);
    await this.fsp.unlink(this.abs(path));
    this.names.removed(path);
  }

  async trash(path: string): Promise<void> {
    await this.inside(path);
    this.names.removed(path);
    if (this.opts.trash) return this.opts.trash(path);
    const dir = this.stored("trash");
    await this.fsp.mkdir(dir, { recursive: true });
    await this.fsp.rename(this.abs(path), `${dir}/${Date.now()}-${nameOf(tempBase(path))}`);
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
      await writeFully(fh, data, offset);
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

  async tempCommit(name: string, path: string, expect: Expect): Promise<WriteOutcome> {
    await this.inside(path);
    if (!satisfies(await this.stat(path), expect)) return { kind: "precondition" };
    await this.fsp.mkdir(this.abs(parentOf(path)), { recursive: true });
    return this.place(this.stored(`tmp/${name}`), path, expect, false);
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

/**
 * Папки vault'а, которые недавно были обычными (не символическими ссылками), — чтобы
 * не проверять каждую папку пути на каждой операции. Свои удаления и переименования
 * вносятся сразу; чужая замена папки ссылкой видна не позже чем через `ttlMs`.
 */
class RealDirs {
  private dirs = new Map<string, number>();

  constructor(
    private ttlMs = 2000,
    private now: () => number = Date.now,
  ) {}

  known(dir: string): boolean {
    const at = this.dirs.get(dir);
    return at !== undefined && this.now() - at < this.ttlMs;
  }

  mark(dir: string): void {
    this.dirs.set(dir, this.now());
  }

  removed(path: string): void {
    for (const d of [...this.dirs.keys()]) if (d === path || d.startsWith(`${path}/`)) this.dirs.delete(d);
  }

  clear(): void {
    this.dirs.clear();
  }
}

/** Файла (или пути к нему) нет — `null`, остальные ошибки пробрасываются. */
function missing(e: unknown): null {
  const c = code(e);
  if (c === "ENOENT" || c === "ENOTDIR") return null;
  throw e;
}

function code(e: unknown): string | undefined {
  return (e as { code?: string }).code;
}

/** ФС не умеет жёстких ссылок (FAT/exFAT, часть сетевых и облачных ФС). */
function noLinks(e: unknown): boolean {
  return ["EPERM", "ENOTSUP", "EOPNOTSUPP", "EXDEV", "EMLINK", "ENOSYS", "EINVAL", "EACCES"].includes(code(e) ?? "");
}

function fileMeta(path: string, st: Stats): FileMeta {
  return { path, size: st.size, mtime: Math.trunc(st.mtimeMs), dir: false, ctime: Math.trunc(st.ctimeMs) };
}

/** Запись с fsync: после rename поверх старого файла не останется пустышки. */
async function writeDurable(fsp: FsPromises, file: string, data: Uint8Array): Promise<void> {
  const fh = await fsp.open(file, "w");
  try {
    await writeFully(fh, data, 0);
    await fh.sync();
  } finally {
    await fh.close();
  }
}

type FileHandle = Awaited<ReturnType<FsPromises["open"]>>;

/** Весь буфер с позиции `pos`: один `write` может записать меньше (диск или квота
 *  заполнены, сетевая ФС) и не сообщить об ошибке — дописывать до конца. */
async function writeFully(fh: FileHandle, data: Uint8Array, pos: number): Promise<void> {
  let off = 0;
  while (off < data.length) {
    const { bytesWritten } = await fh.write(data, off, data.length - off, pos + off);
    if (bytesWritten <= 0) throw new Error(`запись не продвигается: ${off} из ${data.length} байт`);
    off += bytesWritten;
  }
}

async function readRange(fsp: FsPromises, file: string, offset: number, len?: number): Promise<Uint8Array | null> {
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
