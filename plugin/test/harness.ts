// Обвязка e2e-тестов: настоящий сервер `selfsync serve` во временном каталоге и
// клиенты плагина (Runner + Executor + бэкенд) без Obsidian.

import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import * as fsp from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Runner, initWasm } from "../src/engine.ts";
import { type Connection, Executor } from "../src/executor.ts";
import type { FileBackend, HttpBackend, HttpResponse } from "../src/io/backend.ts";
import { body, headerRecord } from "../src/io/http.ts";
import { NodeBackend } from "../src/io/node.ts";
import type { EngineConfig, Notice, SyncStatus } from "../src/types.ts";

// Код плагина ставит таймеры через window (как требует Obsidian для всплывающих окон).
(globalThis as Record<string, unknown>).window ??= globalThis;

const ROOT = new URL("../..", import.meta.url).pathname;
export const SERVER_BIN = process.env.SELFSYNC_BIN ?? join(ROOT, "target/debug/selfsync");

let wasmLoaded = false;
async function loadWasm(): Promise<void> {
  if (wasmLoaded) return;
  initWasm(await fsp.readFile(join(ROOT, "plugin/pkg/selfsync_wasm_bg.wasm")));
  wasmLoaded = true;
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.listen(0, "127.0.0.1", () => {
      const addr = s.address();
      const port = typeof addr === "object" && addr ? addr.port : 0;
      s.close(() => resolve(port));
    });
    s.on("error", reject);
  });
}

export class TestServer {
  private constructor(
    readonly url: string,
    readonly data: string,
    private proc: ChildProcess,
  ) {}

  static async start(): Promise<TestServer> {
    await fsp.access(SERVER_BIN).catch(() => {
      throw new Error(`нет бинаря сервера ${SERVER_BIN}: соберите cargo build -p selfsync-server`);
    });
    const data = await fsp.mkdtemp(join(tmpdir(), "selfsync-e2e-"));
    const port = await freePort();
    const proc = spawn(SERVER_BIN, ["--data", data, "serve", "--listen", `127.0.0.1:${port}`], {
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env, RUST_LOG: "warn" },
    });
    const url = `http://127.0.0.1:${port}`;
    for (let i = 0; i < 100; i++) {
      const ok = await fetch(`${url}/v1/health`).then((r) => r.ok, () => false);
      if (ok) return new TestServer(url, data, proc);
      await new Promise((r) => setTimeout(r, 50));
    }
    proc.kill();
    throw new Error("сервер не поднялся");
  }

  /** Служебная команда сервера (token add, link…), stdout. */
  cli(...args: string[]): string {
    const r = spawnSync(SERVER_BIN, ["--data", this.data, ...args], { encoding: "utf8" });
    if (r.status !== 0) throw new Error(`selfsync ${args.join(" ")}: ${r.stderr}`);
    return r.stdout;
  }

  /** Токен нового устройства (последняя непустая строка вывода `token add`). */
  token(vault: string, name: string): string {
    const lines = this.cli("token", "add", "--vault", vault, "--name", name).trim().split("\n");
    return lines[lines.length - 1] ?? "";
  }

  async stop(): Promise<void> {
    this.proc.kill();
    await fsp.rm(this.data, { recursive: true, force: true });
  }
}

export interface ClientOptions {
  name: string;
  token?: string;
  config?: EngineConfig;
  /** Бэкенд вместо Node fs (например, адаптер Obsidian поверх заглушки). */
  backend?: (vault: string, store: string) => FileBackend;
  key?: Uint8Array;
}

export class TestClient {
  notices: Notice[] = [];
  status: SyncStatus | undefined;
  logs: string[] = [];
  rememberedKey: Uint8Array | undefined;
  runner!: Runner;

  private constructor(
    readonly name: string,
    readonly vault: string,
    readonly store: string,
    readonly fs: FileBackend,
    readonly conn: Connection,
  ) {}

  static async create(server: TestServer, o: ClientOptions): Promise<TestClient> {
    await loadWasm();
    const base = await fsp.mkdtemp(join(tmpdir(), `selfsync-${o.name}-`));
    const vault = join(base, "vault");
    const store = join(base, "store");
    await fsp.mkdir(vault, { recursive: true });
    await fsp.mkdir(store, { recursive: true });
    const fs = o.backend ? o.backend(vault, store) : new NodeBackend(vault, store, fsp, { caseSensitive: true });
    const c = new TestClient(o.name, vault, store, fs, { server: server.url, token: o.token });
    await c.start(o.config ?? {}, o.key);
    return c;
  }

  /** (Пере)запуск движка с сохранённым индексом — как перезапуск плагина. */
  async restart(config: EngineConfig = {}): Promise<void> {
    this.runner.stop();
    await this.start(config, this.rememberedKey);
  }

  private async start(config: EngineConfig, key?: Uint8Array): Promise<void> {
    const exec = new Executor(this.fs, new FetchHttp(), this.conn);
    const index = await exec.loadIndex();
    this.runner = new Runner({ deviceName: this.name, ...config }, index, exec, {
      status: (s) => (this.status = s),
      notice: (n) => this.notices.push(n),
      log: (level, m) => this.logs.push(`${level}: ${m}`),
      rememberKey: (k) => (this.rememberedKey = k),
      forgetKey: () => (this.rememberedKey = undefined),
    });
    this.runner.send({ type: "start", key: key ?? null });
    await this.runner.idle();
  }

  /** Синк сейчас и ожидание, пока всё выполнено. */
  async sync(): Promise<void> {
    this.runner.send({ type: "syncNow" });
    await this.runner.idle();
  }

  async write(path: string, text: string | Uint8Array): Promise<void> {
    const full = join(this.vault, path);
    await fsp.mkdir(join(full, ".."), { recursive: true });
    await fsp.writeFile(full, text);
  }

  async read(path: string): Promise<string | null> {
    return fsp.readFile(join(this.vault, path), "utf8").catch(() => null);
  }

  /** Все файлы vault'а (без служебных), путь → содержимое. */
  async files(): Promise<Map<string, string>> {
    const out = new Map<string, string>();
    for (const m of await this.fs.list()) {
      if (m.dir) continue;
      out.set(m.path, (await this.read(m.path)) ?? "");
    }
    return out;
  }

  async stop(): Promise<void> {
    this.runner?.stop();
    await fsp.rm(join(this.vault, ".."), { recursive: true, force: true });
  }
}

/** HTTP в тестах — fetch из Node. */
export class FetchHttp implements HttpBackend {
  async request(url: string, method: string, headers: [string, string][], data: Uint8Array, timeoutMs: number): Promise<HttpResponse> {
    const r = await fetch(url, {
      method,
      headers: headerRecord(headers),
      body: body(data),
      signal: AbortSignal.timeout(timeoutMs),
    });
    const out: [string, string][] = [];
    r.headers.forEach((v, k) => out.push([k, v]));
    return { status: r.status, headers: out, body: new Uint8Array(await r.arrayBuffer()) };
  }
}
