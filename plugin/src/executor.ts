// Исполнитель действий ядра: превращает Action в вызовы бэкендов и возвращает
// IoResult. Решений о синке здесь нет — только честное выполнение контракта из
// types.rs (условия записи, коды Precondition/NotFound, настоящие имена файлов).

import type { FileBackend, HttpBackend } from "./io/backend.ts";
import type { Expect, FileMeta, IoAction, IoResult } from "./types.ts";

const INDEX_FILE = "index.bin";
const CACHE_DIR = "cache";

export interface Connection {
  /** Базовый адрес сервера без завершающего «/», например `https://host/selfsync`. */
  server: string;
  /** Токен устройства; без него запросы с `auth` уходят без заголовка. */
  token?: string;
}

export class Executor {
  constructor(
    private fs: FileBackend,
    private http: HttpBackend,
    private conn: Connection,
  ) {}

  async loadIndex(): Promise<Uint8Array | null> {
    return this.fs.storeRead(INDEX_FILE);
  }

  async perform(a: IoAction): Promise<IoResult> {
    try {
      return await this.dispatch(a);
    } catch (e) {
      return { type: "failed", message: e instanceof Error ? e.message : String(e) };
    }
  }

  private async dispatch(a: IoAction): Promise<IoResult> {
    switch (a.type) {
      case "http": {
        const headers: [string, string][] = [...a.req.headers];
        if (a.req.auth && this.conn.token) headers.push(["authorization", `Bearer ${this.conn.token}`]);
        const r = await this.http.request(this.conn.server + a.req.path, a.req.method, headers, a.req.body, a.req.timeoutMs);
        return { type: "http", status: r.status, headers: r.headers, body: r.body };
      }
      case "list":
        return { type: "listing", files: await this.fs.list() };
      case "stat":
        return { type: "stat", meta: await this.fs.stat(a.path) };
      case "read": {
        const data = await this.fs.read(a.path, a.offset, a.len ?? undefined);
        return data ? { type: "data", data } : { type: "notFound" };
      }
      case "write": {
        if (!(await this.holds(a.path, a.expect))) return { type: "precondition" };
        const cur = await this.fs.stat(a.path);
        if (cur?.dir) return { type: "precondition" };
        return { type: "stat", meta: await this.fs.writeAtomic(a.path, a.data) };
      }
      case "writeTemp":
        await this.fs.tempWrite(a.temp, a.offset, a.data);
        return { type: "done" };
      case "readTemp": {
        const data = await this.fs.tempRead(a.temp, a.offset, a.len);
        return data ? { type: "data", data } : { type: "notFound" };
      }
      case "commitTemp": {
        if (!(await this.holds(a.path, a.expect))) return { type: "precondition" };
        return { type: "stat", meta: await this.fs.tempCommit(a.temp, a.path) };
      }
      case "deleteTemp":
        await this.fs.tempDelete(a.temp);
        return { type: "done" };
      case "trash": {
        const cur = await this.fs.stat(a.path);
        // Файла нет — NotFound при любом expect: удалять нечего.
        if (!cur) return { type: "notFound" };
        if (cur.dir || !matches(cur, a.expect)) return { type: "precondition" };
        await this.fs.trash(cur.path);
        return { type: "done" };
      }
      case "rename": {
        const src = await this.fs.stat(a.from);
        if (!src) return { type: "notFound" };
        const dst = await this.fs.stat(a.to);
        // Занято другим файлом. Тот же файл под другим регистром — не помеха.
        if (dst && dst.path !== src.path) return { type: "precondition" };
        await this.fs.rename(src.path, a.to);
        return { type: "stat", meta: await this.fs.stat(a.to) };
      }
      case "mkdir":
        await this.fs.mkdir(a.path);
        return { type: "done" };
      case "rmdir": {
        const cur = await this.fs.stat(a.path);
        if (!cur) return { type: "notFound" };
        return (await this.fs.rmdir(cur.path)) ? { type: "done" } : { type: "precondition" };
      }
      case "saveIndex":
        await this.fs.storeWrite(INDEX_FILE, a.data);
        return { type: "done" };
      case "cacheRead": {
        const data = await this.fs.storeRead(`${CACHE_DIR}/${a.key}`);
        return data ? { type: "data", data } : { type: "notFound" };
      }
      case "cacheWrite":
        await this.fs.storeWrite(`${CACHE_DIR}/${a.key}`, a.data);
        return { type: "done" };
      case "cacheDelete":
        await this.fs.storeDelete(`${CACHE_DIR}/${a.key}`);
        return { type: "done" };
    }
  }

  /** Условие записи на текущее состояние пути. */
  private async holds(path: string, e: Expect): Promise<boolean> {
    if (e.kind === "any") return true;
    const cur = await this.fs.stat(path);
    if (e.kind === "absent") return cur === null;
    return cur !== null && matches(cur, e);
  }
}

function matches(m: FileMeta, e: Expect): boolean {
  switch (e.kind) {
    case "any":
      return true;
    case "absent":
      return false;
    case "stat":
      return !m.dir && m.size === e.size && m.mtime === e.mtime;
  }
}
