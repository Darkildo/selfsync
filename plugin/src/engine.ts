// Связка ядра (WASM) с исполнителем и таймерами: события → ядро → действия →
// исполнитель → события. Вся логика синка — в ядре; здесь только доставка.

import { initSync, WasmEngine } from "../pkg/selfsync_wasm.js";
import type { Executor } from "./executor.ts";
import { inflateRaw } from "./inflate.ts";
import type { Action, EngineConfig, EngineEvent, LogLevel, Notice, SyncStatus, UiCommand, UiResult } from "./types.ts";

let wasmReady = false;

/** Начало любого WASM-модуля: "\0asm". */
function isWasm(b: Uint8Array): boolean {
  return b[0] === 0 && b[1] === 0x61 && b[2] === 0x73 && b[3] === 0x6d;
}

/**
 * Инициализация WASM из байтов модуля (в плагине они встроены в main.js). Байты —
 * сам модуль или он же, сжатый deflate (так его кладёт сборка, src/build/pack.mjs).
 */
export function initWasm(bytes: Uint8Array): void {
  if (wasmReady) return;
  initSync({ module: isWasm(bytes) ? bytes : inflateRaw(bytes, bytes.length * 3) });
  wasmReady = true;
}

export interface RunnerHooks {
  status?(s: SyncStatus): void;
  notice?(n: Notice): void;
  log?(level: LogLevel, message: string): void;
  rememberKey?(key: Uint8Array): void;
  forgetKey?(): void;
  /** Ядро упало (паника WASM): Runner уже остановлен, движок надо создать заново. */
  fatal?(error: unknown): void;
}

/** То, чем Runner пользуется у движка (в тестах подменяется). */
export interface EngineLike {
  handle(now: number, ev: EngineEvent): unknown;
  nextWake(): number | undefined;
  status(): unknown;
  hasPending(): boolean;
  free(): void;
}

function wasmEngine(config: EngineConfig, index: Uint8Array | null): EngineLike {
  return new WasmEngine(config, index ?? undefined);
}

/**
 * Паника в ядре: WASM собран с `panic = "abort"`, и паника — это ловушка
 * `unreachable`. Состояние движка после неё не определено (занятые RefCell), так
 * что каждое следующее событие падало бы снова, а статус застыл бы.
 */
function isTrap(e: unknown): boolean {
  return typeof WebAssembly !== "undefined" && e instanceof WebAssembly.RuntimeError;
}

/** Самый длинный интервал, который принимает setTimeout. */
const MAX_DELAY = 2 ** 31 - 1;

export class Runner {
  private engine: EngineLike;
  private timer: number | undefined;
  private timerAt = 0;
  private inflight = 0;
  private stopped = false;
  private nextReq = 1;
  private waiting = new Map<number, (r: UiResult) => void>();
  private idleWaiters: (() => void)[] = [];

  constructor(
    config: EngineConfig,
    index: Uint8Array | null,
    private exec: Executor,
    private hooks: RunnerHooks = {},
    private clock: () => number = Date.now,
    make: (config: EngineConfig, index: Uint8Array | null) => EngineLike = wasmEngine,
  ) {
    this.engine = make(config, index);
  }

  /** Передать событие ядру и запустить полученные действия. */
  send(ev: EngineEvent): void {
    if (this.stopped) return;
    let actions: Action[];
    try {
      actions = this.engine.handle(this.clock(), ev) as Action[];
    } catch (e) {
      // Иначе ждущий ответа на команду (окно интерфейса) повис бы навсегда.
      if (ev.type === "command") this.reply(ev.req, { type: "error", code: "rejected", message: String(e) });
      // Команду с негодными данными ядро отвергает, не ломаясь; прочее — сбой ядра.
      if (ev.type === "command" && !isTrap(e)) {
        this.hooks.log?.("warn", `ядро отклонило команду: ${String(e)}`);
        return;
      }
      this.hooks.log?.("error", `сбой ядра на событии ${ev.type}: ${String(e)}`);
      this.stop();
      this.hooks.fatal?.(e);
      return;
    }
    for (const a of actions) this.run(a);
    // Таймер мог сработать раньше срока (часы, сон устройства), а Wake ядро шлёт
    // только при изменении момента — перевзвести по его текущему желанию.
    const next = this.engine.nextWake();
    if (next !== undefined && (this.timer === undefined || next < this.timerAt)) this.schedule(next);
    this.checkIdle();
  }

  private run(a: Action): void {
    switch (a.type) {
      case "wake":
        this.schedule(a.at);
        return;
      case "status":
        this.hooks.status?.(a.status);
        return;
      case "notify":
        this.hooks.notice?.(a.notice);
        return;
      case "log":
        this.hooks.log?.(a.level, a.message);
        return;
      case "rememberKey":
        this.hooks.rememberKey?.(a.key);
        return;
      case "forgetKey":
        this.hooks.forgetKey?.();
        return;
      case "uiResult":
        this.reply(a.req, a.result);
        return;
      default: {
        this.inflight++;
        void this.exec.perform(a).then((result) => {
          this.inflight--;
          if (this.stopped) {
            this.checkIdle();
            return;
          }
          this.send({ type: "done", id: a.id, result });
        });
      }
    }
  }

  private schedule(at: number): void {
    if (this.timer !== undefined) window.clearTimeout(this.timer);
    this.timerAt = at;
    const delay = Math.min(MAX_DELAY, Math.max(0, at - this.clock()));
    this.timer = window.setTimeout(() => {
      this.timer = undefined;
      this.send({ type: "tick" });
    }, delay);
  }

  /** Команда интерфейса; ответ — UiResult. */
  private reply(req: number, result: UiResult): void {
    const w = this.waiting.get(req);
    this.waiting.delete(req);
    w?.(result);
  }

  command(command: UiCommand): Promise<UiResult> {
    const req = this.nextReq++;
    return new Promise((resolve) => {
      this.waiting.set(req, resolve);
      this.send({ type: "command", req, command });
    });
  }

  /** Все запущенные действия выполнены (таймеры не в счёт). */
  idle(): Promise<void> {
    if (this.inflight === 0) return Promise.resolve();
    return new Promise((resolve) => this.idleWaiters.push(resolve));
  }

  private checkIdle(): void {
    if (this.inflight !== 0) return;
    const ws = this.idleWaiters;
    this.idleWaiters = [];
    for (const w of ws) w();
  }

  status(): SyncStatus {
    return this.engine.status() as SyncStatus;
  }

  /** Остановлен (вручную или после сбоя ядра). */
  get isStopped(): boolean {
    return this.stopped;
  }

  hasPending(): boolean {
    return this.engine.hasPending();
  }

  /** Остановить: таймеры снимаются, ответы на уже запущенные действия игнорируются. */
  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    if (this.timer !== undefined) window.clearTimeout(this.timer);
    this.timer = undefined;
    for (const w of this.waiting.values()) w({ type: "error", code: "stopped", message: "синк остановлен" });
    this.waiting.clear();
    try {
      this.engine.free();
    } catch {
      // После сбоя ядра освобождение тоже может упасть: движок всё равно брошен.
    }
    this.checkIdle();
  }
}
