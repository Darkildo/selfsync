// Настройки плагина и их вкладка.

import { type App, Notice, PluginSettingTab, Setting } from "obsidian";

import { t } from "./i18n.ts";
import type SelfsyncPlugin from "./main.ts";
import { ConnectModal } from "./ui/connect.ts";
import { JoinModal } from "./ui/join.ts";
import { confirm } from "./ui/confirm.ts";
import { ChangePasswordModal, PasswordModal } from "./ui/password.ts";

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
  // По умолчанию — каталог настроек vault'а, его подставляет loadSettings.
  excludes: [],
  useWait: false,
  debounceSec: 2.5,
  pollActiveSec: 15,
  pollIdleMaxMin: 5,
};

export function normalizeServer(url: string): string {
  return url.trim().replace(/\/+$/, "");
}

export class SelfsyncSettingTab extends PluginSettingTab {
  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
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

    // Остальное — только для подключённого устройства: это команды серверу.
    if (!this.plugin.runner) return;
    this.encryption();
    this.retention();
    this.devices();
  }

  private encryption(): void {
    const runner = this.plugin.runner;
    if (!runner) return;
    const encrypted = runner.status().encrypted;
    new Setting(this.containerEl).setName(t("encryption.heading")).setHeading();
    const s = new Setting(this.containerEl).setDesc(encrypted ? t("encryption.on") : t("encryption.off"));
    if (!encrypted) {
      s.addButton((b) =>
        b
          .setButtonText(t("encryption.enable"))
          .setWarning()
          .onClick(() =>
            new PasswordModal(
              this.app,
              { title: t("encryption.enable"), desc: t("encryption.enableDesc"), submit: t("encryption.enable"), showStrength: true },
              (r) => {
                runner.send({ type: "enableEncryption", password: r.password, remember: r.remember });
                runner.send({ type: "syncNow" });
              },
            ).open(),
          ),
      );
    } else {
      s.addButton((b) =>
        b.setButtonText(t("encryption.change")).onClick(() =>
          new ChangePasswordModal(this.app, (old, next) => runner.send({ type: "changePassword", old, new: next })).open(),
        ),
      );
    }
  }

  private retention(): void {
    const runner = this.plugin.runner;
    if (!runner) return;
    const setting = new Setting(this.containerEl).setName(t("retention.name")).setDesc(t("retention.desc"));
    setting.addText((x) => {
      x.inputEl.type = "number";
      x.setDisabled(true);
      void runner.command({ type: "getRetention" }).then((r) => {
        if (r.type !== "retention") return;
        x.setValue(String(r.days)).setDisabled(false);
      });
      x.onChange(async (raw) => {
        const days = Math.round(Number(raw));
        if (!Number.isFinite(days) || days < 1 || days > 3650) return;
        const r = await runner.command({ type: "setRetention", days });
        if (r.type === "error") new Notice(r.message);
      });
    });
  }

  private devices(): void {
    const runner = this.plugin.runner;
    if (!runner) return;
    new Setting(this.containerEl).setName(t("devices.heading")).setHeading();
    let name = "";
    new Setting(this.containerEl)
      .setName(t("join.title"))
      .setDesc(t("join.nameDesc"))
      .addText((x) => x.setPlaceholder(t("join.namePlaceholder")).onChange((v) => (name = v.trim())))
      .addButton((b) =>
        b
          .setButtonText(t("join.create"))
          .setCta()
          .onClick(async () => {
            const r = await runner.command({ type: "createJoin", name: name || t("join.namePlaceholder") });
            if (r.type === "join") new JoinModal(this.app, r).open();
            else new Notice(r.type === "error" ? r.message : r.type);
          }),
      );
    const list = this.containerEl.createDiv();
    void runner.command({ type: "devices" }).then((r) => {
      if (r.type !== "devices") return;
      for (const d of r.devices) {
        const seen = d.lastSeen > 0 ? new Date(d.lastSeen).toLocaleString() : "—";
        const row = new Setting(list)
          .setName(d.current ? `${d.name} (${t("devices.current")})` : d.name)
          .setDesc(d.revoked ? t("devices.revoked") : t("devices.lastSeen", { when: seen }));
        if (d.revoked) row.settingEl.addClass("selfsync-device-revoked");
        if (!d.current && !d.revoked) {
          row.addButton((b) =>
            b
              .setButtonText(t("devices.revoke"))
              .setWarning()
              .onClick(async () => {
                if (!(await confirm(this.app, t("devices.revokeConfirm", { name: d.name }), t("devices.revoke")))) return;
                const res = await runner.command({ type: "revokeDevice", id: d.id });
                if (res.type === "error") new Notice(res.message);
                this.display();
              }),
          );
        }
      }
    });
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
