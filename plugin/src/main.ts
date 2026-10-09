// Плагин Obsidian: тонкий слой между vault'ом и ядром. Ядро решает, что и когда
// синхронизировать; здесь — события vault'а, исполнитель, статус-бар и окна.

import { FileSystemAdapter, moment, Notice, Platform, Plugin, requestUrl, type TAbstractFile, TFile } from "obsidian";

import wasmBytes from "selfsync-wasm-bytes";
import { initWasm, Runner } from "./engine.ts";
import { Executor } from "./executor.ts";
import { noticeText, setLanguage, stateText, t } from "./i18n.ts";
import { AdapterBackend } from "./io/adapter.ts";
import type { FileBackend } from "./io/backend.ts";
import { ObsidianHttp } from "./io/http.ts";
import { NodeBackend } from "./io/node.ts";
import { DEFAULT_SETTINGS, SelfsyncSettingTab, type Settings } from "./settings.ts";
import type { EngineConfig, LogLevel, NameRules, Notice as EngineNotice, SyncStatus } from "./types.ts";
import { ConflictsModal } from "./ui/conflicts.ts";
import { ConnectModal } from "./ui/connect.ts";
import { DeletedModal } from "./ui/deleted.ts";
import { HistoryModal } from "./ui/history.ts";
import { PasswordModal } from "./ui/password.ts";

const KEY_STORAGE = "selfsync-master-key";
const LOG_LINES = 500;

export default class SelfsyncPlugin extends Plugin {
  override settings: Settings = { ...DEFAULT_SETTINGS };
  runner: Runner | undefined;
  private settingTab: SelfsyncSettingTab | undefined;
  private statusEl!: HTMLElement;
  private status: SyncStatus | null = null;
  private log: string[] = [];
  private migrationNotice: Notice | undefined;

  override async onload(): Promise<void> {
    setLanguage(moment.locale());
    await this.loadSettings();
    initWasm(wasmBytes);

    this.statusEl = this.addStatusBarItem();
    this.statusEl.addClass("selfsync-status", "mod-clickable");
    this.registerDomEvent(this.statusEl, "click", () => this.onStatusClick());
    this.renderStatus();

    this.addCommand({ id: "sync-now", name: t("cmd.syncNow"), callback: () => this.syncNow() });
    this.addCommand({
      id: "connect",
      name: t("cmd.connect"),
      callback: () => new ConnectModal(this.app, this, this.settings.server, "").open(),
    });
    this.addCommand({ id: "password", name: t("cmd.password"), callback: () => this.askPassword() });
    this.addCommand({ id: "conflicts", name: t("cmd.conflicts"), callback: () => this.whenRunning(() => new ConflictsModal(this.app, this).open()) });
    this.addCommand({ id: "deleted", name: t("cmd.deleted"), callback: () => this.whenRunning(() => new DeletedModal(this.app, this).open()) });
    this.addCommand({
      id: "history",
      name: t("cmd.history"),
      checkCallback: (checking) => {
        const file = this.app.workspace.getActiveFile();
        if (!file || !this.runner) return false;
        if (!checking) new HistoryModal(this.app, this, file.path).open();
        return true;
      },
    });
    this.registerEvent(
      this.app.workspace.on("file-menu", (menu, file) => {
        if (!this.runner || !(file instanceof TFile)) return;
        menu.addItem((i) => i.setTitle(t("cmd.history")).setIcon("history").onClick(() => new HistoryModal(this.app, this, file.path).open()));
      }),
    );
    this.addCommand({ id: "log", name: t("cmd.log"), callback: () => this.showLog() });
    this.settingTab = new SelfsyncSettingTab(this.app, this);
    this.addSettingTab(this.settingTab);

    // Ссылка со страницы /join/{code} сервера.
    this.registerObsidianProtocolHandler("selfsync-connect", (p) => {
      new ConnectModal(this.app, this, p.server ?? "", p.code ?? "").open();
    });

    this.app.workspace.onLayoutReady(() => {
      // События vault'а — только после загрузки: до неё create приходит на каждый файл.
      const changed = (f: TAbstractFile) => this.runner?.send({ type: "changed", path: f.path });
      this.registerEvent(this.app.vault.on("create", changed));
      this.registerEvent(this.app.vault.on("modify", changed));
      this.registerEvent(this.app.vault.on("delete", (f) => this.runner?.send({ type: "deleted", path: f.path })));
      this.registerEvent(this.app.vault.on("rename", (f, old) => this.runner?.send({ type: "renamed", from: old, to: f.path })));
      this.registerDomEvent(document, "visibilitychange", () => {
        this.runner?.send({ type: document.visibilityState === "hidden" ? "hidden" : "visible" });
      });
      void this.start();
    });
  }

  override onunload(): void {
    this.runner?.stop();
    this.runner = undefined;
  }

  async loadSettings(): Promise<void> {
    const saved = (await this.loadData()) as Partial<Settings> | null;
    this.settings = { ...DEFAULT_SETTINGS, ...saved };
    // Каталог настроек Obsidian не обязательно `.obsidian`: его можно переименовать.
    if (!saved?.excludes) this.settings.excludes = [`${this.app.vault.configDir}/`];
    if (!this.settings.deviceName) this.settings.deviceName = defaultDeviceName();
  }

  /** Сохранить настройки; при смене сервера/токена — перезапуск, иначе новая конфигурация ядру. */
  async saveSettings(o: { restart?: boolean; reconfigure?: boolean } = {}): Promise<void> {
    await this.saveData(this.settings);
    if (o.restart) await this.start();
    else if (o.reconfigure) this.runner?.send({ type: "configure", config: this.engineConfig() });
  }

  private get pluginDir(): string {
    return this.manifest.dir ?? `${this.app.vault.configDir}/plugins/${this.manifest.id}`;
  }

  engineConfig(): EngineConfig {
    return {
      deviceName: this.settings.deviceName,
      excludes: this.settings.excludes,
      // Каталог плагина (индекс, кэш, токен) и локальная корзина Obsidian — никогда.
      hardExcludes: [`${this.pluginDir}/`, ".trash/"],
      caseInsensitive: !Platform.isLinux,
      nameRules: nameRules(),
      debounceMs: Math.round(this.settings.debounceSec * 1000),
      pollActiveMs: Math.round(this.settings.pollActiveSec * 1000),
      pollIdleMaxMs: Math.round(this.settings.pollIdleMaxMin * 60_000),
      useWait: this.settings.useWait,
      tzOffsetMin: -new Date().getTimezoneOffset(),
    };
  }

  private backend(): FileBackend {
    const adapter = this.app.vault.adapter;
    if (Platform.isDesktop && adapter instanceof FileSystemAdapter) {
      // eslint-disable-next-line no-undef, @typescript-eslint/no-require-imports -- Node fs есть только в десктопном Obsidian (там у плагина CommonJS require): подгружается здесь, а не при загрузке main.js
      const fsp = require("fs/promises") as typeof import("node:fs/promises");
      return new NodeBackend(adapter.getBasePath(), adapter.getFullPath(this.pluginDir), fsp, {
        caseSensitive: Platform.isLinux,
        trash: (p) => this.trash(p),
      });
    }
    return new AdapterBackend(adapter, this.pluginDir, this.trashMode());
  }

  /** Как удаляет сам Obsidian: системная корзина или `.trash`; безвозвратно — никогда. */
  private trashMode(): "system" | "local" {
    const vault = this.app.vault as unknown as { getConfig?: (k: string) => unknown };
    return vault.getConfig?.("trashOption") === "system" ? "system" : "local";
  }

  private async trash(path: string): Promise<void> {
    const a = this.app.vault.adapter;
    if (this.trashMode() === "system" && (await a.trashSystem(path))) return;
    await a.trashLocal(path);
  }

  private executor(server: string, token?: string): Executor {
    return new Executor(this.backend(), new ObsidianHttp(requestUrl), { server, token });
  }

  /** (Пере)запуск синка с текущими настройками. */
  async start(): Promise<void> {
    this.runner?.stop();
    this.runner = undefined;
    this.status = null;
    if (!this.settings.server || !this.settings.token) {
      this.renderStatus();
      return;
    }
    const exec = this.executor(this.settings.server, this.settings.token);
    const index = await exec.loadIndex();
    this.runner = new Runner(this.engineConfig(), index, exec, {
      status: (s) => {
        const wasEncrypted = this.status?.encrypted;
        this.status = s;
        this.renderStatus();
        // Шифрование включили (здесь или на другом устройстве) — иначе в открытых
        // настройках так и висела бы кнопка «Включить».
        if (wasEncrypted !== undefined && wasEncrypted !== s.encrypted) this.settingTab?.refresh();
      },
      notice: (n) => this.showNotice(n),
      log: (level, m) => this.addLog(level, m),
      rememberKey: (k) => this.app.saveLocalStorage(KEY_STORAGE, toBase64(k)),
      forgetKey: () => this.app.saveLocalStorage(KEY_STORAGE, null),
    });
    const saved = this.app.loadLocalStorage(KEY_STORAGE) as string | null;
    this.runner.send({ type: "start", key: saved ? fromBase64(saved) : null });
  }

  private whenRunning(f: () => void): void {
    if (this.runner) f();
    else new ConnectModal(this.app, this, this.settings.server, "").open();
  }

  syncNow(): void {
    if (this.runner) this.runner.send({ type: "syncNow" });
    else new ConnectModal(this.app, this, this.settings.server, "").open();
  }

  /** Обменять одноразовый код на токен. Возвращает текст ошибки или null. */
  async redeem(server: string, code: string, device: string): Promise<string | null> {
    if (!server || !code) return "server/code";
    // Временный движок без токена: команда redeem — только HTTP, синк не запускается.
    const tmp = new Runner({ deviceName: device }, null, this.executor(server), { log: (l, m) => this.addLog(l, m) });
    try {
      const r = await tmp.command({ type: "redeem", code, name: device });
      if (r.type === "error") return r.message;
      if (r.type !== "token") return r.type;
      this.settings.server = server;
      this.settings.token = r.token;
      this.settings.vault = r.vault;
      this.settings.deviceName = r.deviceName || device;
      await this.saveSettings({ restart: true });
      return null;
    } finally {
      tmp.stop();
    }
  }

  private onStatusClick(): void {
    const s = this.status;
    if (s?.state === "needPassword") this.askPassword();
    else if (s?.state === "blocked" && s.reason === "unauthorized") new ConnectModal(this.app, this, this.settings.server, "").open();
    else this.syncNow();
  }

  askPassword(): void {
    new PasswordModal(
      this.app,
      { title: t("password.title"), desc: t("password.unlockDesc"), submit: t("password.submit"), showStrength: false },
      (r) => this.runner?.send({ type: "password", password: r.password, remember: r.remember }),
    ).open();
  }

  private renderStatus(): void {
    const s = this.status;
    const icon = !s ? "○" : { idle: "✓", syncing: "↻", offline: "⚠", error: "✗", needPassword: "🔒", blocked: "⛔" }[s.state];
    const parts = [`${icon} ${stateText(s)}`];
    if (s?.state === "syncing" && s.total > 0) parts.push(`${s.done}/${s.total}`);
    else if (s && s.pending > 0) parts.push(t("status.pending", { n: s.pending }));
    if (s && s.conflicts > 0) parts.push(t("status.conflicts", { n: s.conflicts }));
    this.statusEl.setText(parts.join(" · "));
    let tip = t("status.tooltip", { state: stateText(s) });
    if (s && s.lastSync > 0) tip += `\n${t("status.lastSync", { time: new Date(s.lastSync).toLocaleTimeString() })}`;
    this.statusEl.setAttribute("aria-label", tip);
  }

  private showNotice(n: EngineNotice): void {
    this.addLog("info", `notice ${JSON.stringify(n)}`);
    const text = noticeText(n);
    if (!text) return;
    if (n.kind === "migrationProgress") {
      // Одно обновляемое уведомление вместо потока.
      if (!this.migrationNotice) this.migrationNotice = new Notice(text, 0);
      else this.migrationNotice.setMessage(text);
      if (n.done >= n.total) this.migrationNotice = undefined;
      return;
    }
    if (n.kind === "encryptionEnabled") {
      this.migrationNotice?.hide();
      this.migrationNotice = undefined;
    }
    const shown = new Notice(text, n.kind === "conflict" || n.kind === "wrongPassword" ? 0 : 8000);
    if (n.kind === "conflict") shown.messageEl.onClickEvent(() => new ConflictsModal(this.app, this).open());
  }

  private addLog(level: LogLevel, message: string): void {
    if (level === "debug") return;
    this.log.push(`${new Date().toISOString()} ${level} ${message}`);
    if (this.log.length > LOG_LINES) this.log.splice(0, this.log.length - LOG_LINES);
    if (level === "error" || level === "warn") console.warn(`selfsync: ${message}`);
  }

  private showLog(): void {
    const text = this.log.slice(-50).join("\n") || "—";
    void navigator.clipboard?.writeText(this.log.join("\n"));
    new Notice(text, 15000);
  }
}

/** Каких имён не создать на этой платформе (`?` и т.п. — на Windows и в хранилище Android). */
function nameRules(): NameRules {
  if (Platform.isWin) return "windows";
  if (Platform.isAndroidApp) return "fat";
  return "any";
}

function defaultDeviceName(): string {
  if (Platform.isIosApp) return "iPhone";
  if (Platform.isAndroidApp) return "Android";
  if (Platform.isMacOS) return "Mac";
  if (Platform.isWin) return "Windows";
  return "Linux";
}

function toBase64(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s);
}

function fromBase64(s: string): Uint8Array {
  return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
}

