// Заглушка модуля `obsidian` для запуска собранного main.js в Node: ровно то, что
// трогает плагин, с поведением настоящего API там, где это важно (FileSystemAdapter,
// requestUrl, события vault'а).

import * as fsp from "node:fs/promises";
import { join } from "node:path";

export const shown: string[] = [];

export class Notice {
  constructor(public message: string) {
    shown.push(message);
  }
  setMessage(m: string): this {
    shown.push(m);
    return this;
  }
  hide(): void {}
}

export const Platform = {
  isDesktopApp: true,
  isMobileApp: false,
  isLinux: true,
  isMacOS: false,
  isWin: false,
  isIosApp: false,
  isAndroidApp: false,
};

export const moment = { locale: () => "ru" };

export async function requestUrl(p: {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: ArrayBuffer;
}): Promise<{ status: number; headers: Record<string, string>; arrayBuffer: ArrayBuffer }> {
  const r = await fetch(p.url, { method: p.method, headers: p.headers, body: p.body });
  const headers: Record<string, string> = {};
  r.headers.forEach((v, k) => (headers[k] = v));
  return { status: r.status, headers, arrayBuffer: await r.arrayBuffer() };
}

export class FileSystemAdapter {
  constructor(private base: string) {}
  getBasePath(): string {
    return this.base;
  }
  getFullPath(p: string): string {
    return join(this.base, p);
  }
  async trashSystem(): Promise<boolean> {
    return false;
  }
  async trashLocal(p: string): Promise<void> {
    await fsp.mkdir(join(this.base, ".trash"), { recursive: true });
    await fsp.rename(join(this.base, p), join(this.base, ".trash", p.replaceAll("/", "_")));
  }
}

class El {
  text = "";
  attrs: Record<string, string> = {};
  addClass(): void {}
  setText(t: string): void {
    this.text = t;
  }
  setAttribute(k: string, v: string): void {
    this.attrs[k] = v;
  }
}

type Handler = (...args: unknown[]) => void;

export class Plugin {
  statusBar = new El();
  commands: { id: string; callback: () => void }[] = [];
  private data: unknown;

  constructor(
    public app: StubApp,
    public manifest: { id: string; dir?: string },
  ) {
    this.data = app.pluginData;
  }
  async loadData(): Promise<unknown> {
    return this.data;
  }
  async saveData(d: unknown): Promise<void> {
    this.data = d;
  }
  addStatusBarItem(): El {
    return this.statusBar;
  }
  registerDomEvent(): void {}
  registerEvent(): void {}
  addCommand(c: { id: string; callback: () => void }): void {
    this.commands.push(c);
  }
  addSettingTab(): void {}
  registerObsidianProtocolHandler(): void {}
}

export class PluginSettingTab {
  containerEl = new El();
  constructor(
    public app: unknown,
    public plugin: unknown,
  ) {}
}

export class Modal {
  contentEl = new El();
  constructor(public app: unknown) {}
  open(): void {}
  close(): void {}
  setTitle(): void {}
}

export class Setting {}

export interface StubApp {
  pluginData: unknown;
  vault: {
    adapter: FileSystemAdapter;
    configDir: string;
    handlers: Record<string, Handler[]>;
    on(name: string, cb: Handler): unknown;
    emit(name: string, ...args: unknown[]): void;
    getConfig(k: string): unknown;
  };
  workspace: { onLayoutReady(cb: () => void): void };
  storage: Record<string, unknown>;
  loadLocalStorage(k: string): unknown;
  saveLocalStorage(k: string, v: unknown): void;
}

export function stubApp(vaultDir: string, pluginData: unknown): StubApp {
  const handlers: Record<string, Handler[]> = {};
  const storage: Record<string, unknown> = {};
  return {
    pluginData,
    vault: {
      adapter: new FileSystemAdapter(vaultDir),
      configDir: ".obsidian",
      handlers,
      on(name, cb) {
        (handlers[name] ??= []).push(cb);
        return {};
      },
      emit(name, ...args) {
        for (const h of handlers[name] ?? []) h(...args);
      },
      getConfig: () => "local",
    },
    workspace: { onLayoutReady: (cb) => cb() },
    storage,
    loadLocalStorage: (k) => storage[k] ?? null,
    saveLocalStorage: (k, v) => {
      storage[k] = v;
    },
  };
}
