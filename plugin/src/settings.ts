// Настройки плагина и их вкладка.

import { type App, PluginSettingTab, Setting } from "obsidian";

import { t } from "./i18n.ts";
import type NotesyncPlugin from "./main.ts";
import { ConnectModal } from "./ui/connect.ts";

export interface Settings {
  /** Базовый адрес сервера без завершающего «/». */
  server: string;
  token: string;
  /** Имя vault'а на сервере (для показа; сервер узнаёт его по токену). */
  vault: string;
  deviceName: string;
  excludes: string[];
  useWait: boolean;
  debounceSec: number;
  pollActiveSec: number;
  pollIdleMaxMin: number;
}

export const DEFAULT_SETTINGS: Settings = {
  server: "",
  token: "",
  vault: "",
  deviceName: "",
  excludes: [".obsidian/"],
  useWait: false,
  debounceSec: 2.5,
  pollActiveSec: 15,
  pollIdleMaxMin: 5,
};

export function normalizeServer(url: string): string {
  return url.trim().replace(/\/+$/, "");
}

export class NotesyncSettingTab extends PluginSettingTab {
  constructor(
    app: App,
    private plugin: NotesyncPlugin,
  ) {
    super(app, plugin);
  }

  override display(): void {
    const { containerEl } = this;
    const s = this.plugin.settings;
    containerEl.empty();

    new Setting(containerEl).setName(t("settings.connection")).setHeading();
    containerEl.createEl("p", {
      cls: "setting-item-description",
      text: s.token ? t("settings.connectedTo", { vault: s.vault || "?", device: s.deviceName }) : t("settings.notConnected"),
    });
    new Setting(containerEl)
      .setName(t("settings.server"))
      .setDesc(t("settings.serverDesc"))
      .addText((x) =>
        x.setValue(s.server).onChange(async (v) => {
          s.server = normalizeServer(v);
          await this.plugin.saveSettings({ restart: true });
        }),
      );
    new Setting(containerEl)
      .setName(t("settings.token"))
      .setDesc(t("settings.tokenDesc"))
      .addText((x) => {
        x.inputEl.type = "password";
        x.setValue(s.token).onChange(async (v) => {
          s.token = v.trim();
          await this.plugin.saveSettings({ restart: true });
        });
      });
    new Setting(containerEl)
      .addButton((b) =>
        b
          .setButtonText(t("settings.connectByCode"))
          .setCta()
          .onClick(() => new ConnectModal(this.app, this.plugin, s.server, "", () => this.display()).open()),
      )
      .addButton((b) =>
        b
          .setButtonText(t("settings.disconnect"))
          .setDisabled(!s.token)
          .onClick(async () => {
            s.token = "";
            s.vault = "";
            await this.plugin.saveSettings({ restart: true });
            this.display();
          }),
      );

    new Setting(containerEl)
      .setName(t("settings.device"))
      .setDesc(t("settings.deviceDesc"))
      .addText((x) =>
        x.setValue(s.deviceName).onChange(async (v) => {
          s.deviceName = v.trim() || s.deviceName;
          await this.plugin.saveSettings({ reconfigure: true });
        }),
      );

    new Setting(containerEl).setName(t("settings.sync")).setHeading();
    new Setting(containerEl)
      .setName(t("settings.excludes"))
      .setDesc(t("settings.excludesDesc"))
      .addTextArea((x) => {
        x.inputEl.rows = 5;
        x.setValue(s.excludes.join("\n")).onChange(async (v) => {
          s.excludes = v
            .split("\n")
            .map((l) => l.trim())
            .filter((l) => l !== "");
          await this.plugin.saveSettings({ reconfigure: true });
        });
      });
    new Setting(containerEl)
      .setName(t("settings.useWait"))
      .setDesc(t("settings.useWaitDesc"))
      .addToggle((x) =>
        x.setValue(s.useWait).onChange(async (v) => {
          s.useWait = v;
          await this.plugin.saveSettings({ reconfigure: true });
        }),
      );
    this.number(t("settings.debounce"), s.debounceSec, 0.5, 60, (v) => (s.debounceSec = v));
    this.number(t("settings.pollActive"), s.pollActiveSec, 5, 600, (v) => (s.pollActiveSec = v));
    this.number(t("settings.pollIdle"), s.pollIdleMaxMin, 1, 60, (v) => (s.pollIdleMaxMin = v));
  }

  private number(name: string, value: number, min: number, max: number, set: (v: number) => void): void {
    new Setting(this.containerEl).setName(name).addText((x) => {
      x.inputEl.type = "number";
      x.setValue(String(value)).onChange(async (raw) => {
        const v = Number(raw);
        if (!Number.isFinite(v)) return;
        set(Math.min(max, Math.max(min, v)));
        await this.plugin.saveSettings({ reconfigure: true });
      });
    });
  }
}
