// Заглушка DataAdapter Obsidian в памяти — с теми же неудобствами, что у
// настоящего: rename не заменяет существующий файл, writeBinary не создаёт папки,
// на регистронезависимой ФС «Note.md» и «note.md» — один файл.

import type { AdapterLike } from "../src/io/adapter.ts";

interface Node {
  name: string;
  dir: boolean;
  data: Uint8Array;
  mtime: number;
}

function parent(p: string): string {
  const i = p.lastIndexOf("/");
  return i < 0 ? "" : p.slice(0, i);
}

export class StubAdapter implements AdapterLike {
  private nodes = new Map<string, Node>();
  private clock = 1_700_000_000_000;
  trashed: string[] = [];
  /** Сколько раз файл переписан целиком (для проверки appendBinary). */
  rewrites = 0;

  constructor(private caseInsensitive = false) {}

  private key(p: string): string {
    const n = p.replace(/^\/+|\/+$/g, "");
    return this.caseInsensitive ? n.toLowerCase() : n;
  }

  private get(p: string): Node | undefined {
    return this.nodes.get(this.key(p));
  }

  private tick(): number {
    this.clock += 1000;
    return this.clock;
  }

  private requireParent(p: string): void {
    const par = parent(p.replace(/^\/+/, ""));
    if (par !== "" && !this.get(par)?.dir) throw new Error(`ENOENT: нет папки ${par}`);
  }

  async exists(p: string, sensitive?: boolean): Promise<boolean> {
    const n = this.get(p);
    if (!n) return false;
    return sensitive ? n.name === p.replace(/^\/+/, "") : true;
  }

  async stat(p: string): Promise<{ type: "file" | "folder"; size: number; mtime: number } | null> {
    const n = this.get(p);
    if (!n) return null;
    return { type: n.dir ? "folder" : "file", size: n.data.length, mtime: n.mtime };
  }

  async list(p: string): Promise<{ files: string[]; folders: string[] }> {
    const dir = this.key(p === "/" ? "" : p);
    const files: string[] = [];
    const folders: string[] = [];
    for (const [k, n] of this.nodes) {
      if (parent(k) !== dir) continue;
      (n.dir ? folders : files).push(n.name);
    }
    return { files, folders };
  }

  async readBinary(p: string): Promise<ArrayBuffer> {
    const n = this.get(p);
    if (!n || n.dir) throw new Error(`ENOENT: ${p}`);
    return n.data.slice().buffer;
  }

  async writeBinary(p: string, data: ArrayBuffer): Promise<void> {
    this.requireParent(p);
    const name = this.get(p)?.name ?? p.replace(/^\/+/, "");
    if (this.get(p)) this.rewrites++;
    this.nodes.set(this.key(p), { name, dir: false, data: new Uint8Array(data.slice(0)), mtime: this.tick() });
  }

  async appendBinary(p: string, data: ArrayBuffer): Promise<void> {
    const n = this.get(p);
    if (!n) return this.writeBinary(p, data);
    const all = new Uint8Array(n.data.length + data.byteLength);
    all.set(n.data, 0);
    all.set(new Uint8Array(data), n.data.length);
    n.data = all;
    n.mtime = this.tick();
  }

  async mkdir(p: string): Promise<void> {
    const clean = p.replace(/^\/+|\/+$/g, "");
    if (clean === "") return;
    const parts = clean.split("/");
    for (let i = 1; i <= parts.length; i++) {
      const sub = parts.slice(0, i).join("/");
      if (!this.get(sub)) this.nodes.set(this.key(sub), { name: sub, dir: true, data: new Uint8Array(), mtime: 0 });
    }
  }

  async rmdir(p: string, recursive: boolean): Promise<void> {
    const l = await this.list(p);
    if (!recursive && (l.files.length > 0 || l.folders.length > 0)) throw new Error("ENOTEMPTY");
    this.nodes.delete(this.key(p));
  }

  async remove(p: string): Promise<void> {
    if (!this.get(p)) throw new Error(`ENOENT: ${p}`);
    this.nodes.delete(this.key(p));
  }

  async rename(from: string, to: string): Promise<void> {
    const n = this.get(from);
    if (!n) throw new Error(`ENOENT: ${from}`);
    // Как в Obsidian: назначение занято — ошибка (кроме смены регистра того же файла).
    if (this.get(to) && this.key(to) !== this.key(from)) throw new Error("Destination file already exists!");
    this.requireParent(to);
    const fromKey = this.key(from);
    const toName = to.replace(/^\/+/, "");
    const moved: [string, Node][] = [];
    for (const [k, node] of this.nodes) {
      if (k === fromKey || k.startsWith(`${fromKey}/`)) moved.push([k, node]);
    }
    for (const [k] of moved) this.nodes.delete(k);
    for (const [k, node] of moved) {
      const rest = k.slice(fromKey.length);
      node.name = toName + node.name.slice(n.name.length);
      this.nodes.set(this.key(toName) + rest, node);
    }
  }

  async trashSystem(_p: string): Promise<boolean> {
    return false;
  }

  async trashLocal(p: string): Promise<void> {
    this.trashed.push(this.get(p)?.name ?? p);
    this.nodes.delete(this.key(p));
  }

  /** Записать файл «пользователем» (для тестов). */
  put(p: string, text: string): void {
    void this.mkdir(parent(p));
    const name = this.get(p)?.name ?? p;
    this.nodes.set(this.key(p), { name, dir: false, data: new TextEncoder().encode(text), mtime: this.tick() });
  }

  text(p: string): string | null {
    const n = this.get(p);
    return n && !n.dir ? new TextDecoder().decode(n.data) : null;
  }

  names(): string[] {
    return [...this.nodes.values()].filter((n) => !n.dir).map((n) => n.name);
  }
}
