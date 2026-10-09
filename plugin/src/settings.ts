// Настройки плагина и их вкладка.

import { type App, Notice, PluginSettingTab, requireApiVersion, Setting, type SettingDefinitionItem } from "obsidian";

import { t } from "./i18n.ts";
import type SelfsyncPlugin from "./main.ts";
import type { DeviceView } from "./types.ts";
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

/** Строка настроек: имя и описание (для поиска Obsidian 1.13+) и построение. */
interface Row {
  name: string;
  desc?: string;
  /** Заполняет строку целиком: имя, описание и элементы управления. Может вернуть уборку. */
  build: (s: Setting) => unknown;
}

interface Section {
  heading: string;
  rows: Row[];
  /** Команды серверу — только у подключённого устройства. */
  needsRunner?: boolean;
}

/**
 * Вкладка настроек. Строки описаны один раз (`sections`): Obsidian до 1.13
 * рисует их через `display()`, с 1.13 — через `getSettingDefinitions()`, и тогда
 * они находятся поиском по настройкам.
 */
export class SelfsyncSettingTab extends PluginSettingTab {
  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
  ) {
    super(app, plugin);
  }

  /** Перерисовать, если вкладка сейчас открыта. */
  refresh(): void {
    if (!this.containerEl.isConnected) return;
    if (requireApiVersion("1.13.0")) this.update();
    else this.display();
  }

  override getSettingDefinitions(): SettingDefinitionItem[] {
    return this.sections().map((sec) => ({
      type: "group" as const,
      heading: sec.heading,
      visible: () => !sec.needsRunner || this.plugin.runner !== undefined,
      items: sec.rows.map((r) => ({
        name: r.name,
        desc: r.desc,
        render: (s: Setting) => {
          const cleanup = r.build(s);
          return typeof cleanup === "function" ? (cleanup as () => void) : undefined;
        },
      })),
    }));
  }

  override display(): void {
    const { containerEl } = this;
    containerEl.empty();
    for (const sec of this.sections()) {
      if (sec.needsRunner && !this.plugin.runner) continue;
      new Setting(containerEl).setName(sec.heading).setHeading();
      for (const r of sec.rows) r.build(new Setting(containerEl));
    }
  }

  private sections(): Section[] {
    return [
      { heading: t("settings.connection"), rows: this.connectionRows() },
      { heading: t("settings.sync"), rows: this.syncRows() },
      { heading: t("encryption.heading"), rows: [this.encryptionRow()], needsRunner: true },
      { heading: t("devices.heading"), rows: [this.retentionRow(), this.joinRow(), this.devicesRow()], needsRunner: true },
    ];
  }

  private connectionRows(): Row[] {
    const s = this.plugin.settings;
    const status = (): string =>
      s.token ? t("settings.connectedTo", { vault: s.vault || "?", device: s.deviceName }) : t("settings.notConnected");
    return [
      {
        name: t("settings.server"),
        desc: t("settings.serverDesc"),
        build: (row) =>
          row
            .setName(t("settings.server"))
            .setDesc(t("settings.serverDesc"))
            .addText((x) =>
              x.setValue(s.server).onChange(async (v) => {
                s.server = normalizeServer(v);
                await this.plugin.saveSettings({ restart: true });
              }),
            ),
      },
      {
        name: t("settings.token"),
        desc: t("settings.tokenDesc"),
        build: (row) =>
          row
            .setName(t("settings.token"))
            .setDesc(t("settings.tokenDesc"))
            .addText((x) => {
              x.inputEl.type = "password";
              x.setValue(s.token).onChange(async (v) => {
                s.token = v.trim();
                await this.plugin.saveSettings({ restart: true });
              });
            }),
      },
      {
        name: t("settings.connectByCode"),
        build: (row) =>
          row
            .setName(status())
            .setDesc("")
            .addButton((b) =>
              b
                .setButtonText(t("settings.connectByCode"))
                .setCta()
                .onClick(() => new ConnectModal(this.app, this.plugin, s.server, "", () => this.refresh()).open()),
            )
            .addButton((b) =>
              b
                .setButtonText(t("settings.disconnect"))
                .setDisabled(!s.token)
                .onClick(async () => {
                  s.token = "";
                  s.vault = "";
                  await this.plugin.saveSettings({ restart: true });
                  this.refresh();
                }),
            ),
      },
      {
        name: t("settings.device"),
        desc: t("settings.deviceDesc"),
        build: (row) =>
          row
            .setName(t("settings.device"))
            .setDesc(t("settings.deviceDesc"))
            .addText((x) =>
              x.setValue(s.deviceName).onChange(async (v) => {
                s.deviceName = v.trim() || s.deviceName;
                await this.plugin.saveSettings({ reconfigure: true });
              }),
            ),
      },
    ];
  }

  private syncRows(): Row[] {
    const s = this.plugin.settings;
    return [
      {
        name: t("settings.excludes"),
        desc: t("settings.excludesDesc"),
        build: (row) =>
          row
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
            }),
      },
      {
        name: t("settings.useWait"),
        desc: t("settings.useWaitDesc"),
        build: (row) =>
          row
            .setName(t("settings.useWait"))
            .setDesc(t("settings.useWaitDesc"))
            .addToggle((x) =>
              x.setValue(s.useWait).onChange(async (v) => {
                s.useWait = v;
                await this.plugin.saveSettings({ reconfigure: true });
              }),
            ),
      },
      this.numberRow(t("settings.debounce"), () => s.debounceSec, 0.5, 60, (v) => (s.debounceSec = v)),
      this.numberRow(t("settings.pollActive"), () => s.pollActiveSec, 5, 600, (v) => (s.pollActiveSec = v)),
      this.numberRow(t("settings.pollIdle"), () => s.pollIdleMaxMin, 1, 60, (v) => (s.pollIdleMaxMin = v)),
    ];
  }

  private encryptionRow(): Row {
    return {
      name: t("encryption.enable"),
      desc: t("encryption.off"),
      build: (row) => {
        const runner = this.plugin.runner;
        if (!runner) return;
        const encrypted = runner.status().encrypted;
        row.setName("").setDesc(encrypted ? t("encryption.on") : t("encryption.off"));
        if (!encrypted) {
          row.addButton((b) =>
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
          row.addButton((b) =>
            b.setButtonText(t("encryption.change")).onClick(() =>
              new ChangePasswordModal(this.app, (old, next) => runner.send({ type: "changePassword", old, new: next })).open(),
            ),
          );
        }
      },
    };
  }

  private retentionRow(): Row {
    return {
      name: t("retention.name"),
      desc: t("retention.desc"),
      build: (row) => {
        const runner = this.plugin.runner;
        if (!runner) return;
        row.setName(t("retention.name")).setDesc(t("retention.desc"));
        row.addText((x) => {
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
      },
    };
  }

  private joinRow(): Row {
    return {
      name: t("join.title"),
      desc: t("join.nameDesc"),
      build: (row) => {
        const runner = this.plugin.runner;
        if (!runner) return;
        let name = "";
        row
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
      },
    };
  }

  /** Устройства vault'а — внутри одной строки: так она не зависит от того, вставлена
   *  ли уже в документ (декларативные настройки строят строки до вставки). */
  private devicesRow(): Row {
    return {
      name: t("devices.heading"),
      build: (row) => {
        const runner = this.plugin.runner;
        if (!runner) return;
        row.settingEl.empty();
        row.settingEl.addClass("selfsync-devices");
        const list = row.settingEl;
        void runner.command({ type: "devices" }).then((r) => {
          if (r.type !== "devices") return;
          for (const d of r.devices) this.deviceRow(list, d);
        });
      },
    };
  }

  private deviceRow(list: HTMLElement, d: DeviceView): void {
    const runner = this.plugin.runner;
    if (!runner) return;
    const seen = d.lastSeen > 0 ? new Date(d.lastSeen).toLocaleString() : "—";
    const row = new Setting(list)
      .setName(d.current ? `${d.name} (${t("devices.current")})` : d.name)
      .setDesc(d.revoked ? t("devices.revoked") : t("devices.lastSeen", { when: seen }));
    if (d.revoked) row.settingEl.addClass("selfsync-device-revoked");
    if (d.current || d.revoked) return;
    row.addButton((b) =>
      b
        .setButtonText(t("devices.revoke"))
        .setWarning()
        .onClick(async () => {
          if (!(await confirm(this.app, t("devices.revokeConfirm", { name: d.name }), t("devices.revoke")))) return;
          const res = await runner.command({ type: "revokeDevice", id: d.id });
          if (res.type === "error") new Notice(res.message);
          this.refresh();
        }),
    );
  }

  private numberRow(name: string, get: () => number, min: number, max: number, set: (v: number) => void): Row {
    return {
      name,
      build: (row) =>
        row.setName(name).addText((x) => {
          x.inputEl.type = "number";
          x.setValue(String(get())).onChange(async (raw) => {
            const v = Number(raw);
            if (!Number.isFinite(v)) return;
            set(Math.min(max, Math.max(min, v)));
            await this.plugin.saveSettings({ reconfigure: true });
          });
        }),
    };
  }
}
