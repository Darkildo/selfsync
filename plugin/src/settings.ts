// Настройки плагина и их вкладка.

import { type App, Notice, PluginSettingTab, requireApiVersion, Setting, type SettingDefinitionItem } from "obsidian";

import { clientOlderThan, readOnlyMinor } from "../pkg/selfsync_wasm.js";
import { buildRules, type ConfigSync, DEFAULT_CONFIG_SYNC, type Preset, PRESETS } from "./configsync.ts";
import { quotaText, storageText, t } from "./i18n.ts";
import type SelfsyncPlugin from "./main.ts";
import type { DeviceView } from "./types.ts";
import { ConnectModal } from "./ui/connect.ts";
import { DiagnoseModal } from "./ui/diagnose.ts";
import { JoinModal } from "./ui/join.ts";
import { confirm } from "./ui/confirm.ts";
import { ChangePasswordModal, PasswordModal } from "./ui/password.ts";
import { PhraseModal } from "./ui/recovery.ts";
import { SkippedModal } from "./ui/skipped.ts";

export interface Settings {
  /** Базовый адрес сервера без завершающего «/». */
  server: string;
  token: string;
  /** Имя vault'а на сервере (для показа; сервер узнаёт его по токену). */
  vault: string;
  deviceName: string;
  excludes: string[];
  /** Какие части каталога настроек Obsidian синхронизировать. */
  configSync: ConfigSync;
  useWait: boolean;
  debounceSec: number;
  pollActiveSec: number;
  pollIdleMaxMin: number;
  /** Не скачивать файлы больше, МБ (0 — без лимита): облегчённое устройство. */
  maxDownloadMb: number;
}

export const DEFAULT_SETTINGS: Settings = {
  server: "",
  token: "",
  vault: "",
  deviceName: "",
  // Каталог настроек исключается в итоговых правилах сам (configsync.ts).
  excludes: [],
  configSync: { ...DEFAULT_CONFIG_SYNC },
  useWait: false,
  debounceSec: 2.5,
  pollActiveSec: 15,
  pollIdleMaxMin: 5,
  maxDownloadMb: 0,
};

/** Первая выпущенная версия, которая открывает запись ключа с фразой (формат 2). */
const RECOVERY_MIN = "0.2.1";
/** Первая версия, которая понимает смену ключа (поколения ключа, запись формата 3). */
const REKEY_MIN = "0.3.0";

/** Текст ошибки команды: известные коды — локализованно. */
function errorText(r: { code: string; message: string }): string {
  if (r.code === "read_only") return t("readOnly.error");
  if (r.code === "wrong_password") return t("recovery.wrongPassword");
  if (r.code === "server_outdated") return t("error.serverOutdated");
  return r.message;
}

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
  /** Следующая ссылка подключения — для устройства только для чтения. */
  private joinReadOnly = false;

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
      { heading: t("configSync.heading"), rows: this.configSyncRows() },
      { heading: t("encryption.heading"), rows: [this.encryptionRow(), this.recoveryRow()], needsRunner: true },
      {
        heading: t("devices.heading"),
        rows: [this.readOnlyRow(), this.quotaRow(), this.storageRow(), this.retentionRow(), this.joinRow(), this.joinReadOnlyRow(), this.devicesRow()],
        needsRunner: true,
      },
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
        name: t("diag.button"),
        desc: t("diag.buttonDesc"),
        build: (row) =>
          row
            .setName(t("diag.button"))
            .setDesc(t("diag.buttonDesc"))
            .addButton((b) =>
              b
                .setButtonText(t("diag.button"))
                .setDisabled(!this.plugin.runner)
                .onClick(() => new DiagnoseModal(this.app, () => this.plugin.diagnose()).open()),
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
      this.numberRow(t("settings.maxDownload"), () => s.maxDownloadMb, 0, 1_000_000, (v) => (s.maxDownloadMb = v), t("settings.maxDownloadDesc")),
      this.skippedRow(),
    ];
  }

  /** Устройство только для чтения: менять vault'у ничего нельзя. */
  private readOnly(): boolean {
    return this.plugin.runner?.status().readOnly ?? false;
  }

  /** Пояснение вместо кнопок на устройстве только для чтения. */
  private readOnlyRow(): Row {
    return {
      name: t("status.readOnly"),
      desc: t("readOnly.desc"),
      build: (row) => {
        if (!this.readOnly()) {
          row.settingEl.hide();
          return;
        }
        row.setName(t("status.readOnly")).setDesc(t("readOnly.desc"));
      },
    };
  }

  /** Место vault'а на сервере: занято, пороги, файлы, ждущие места. */
  private quotaRow(): Row {
    return {
      name: t("quota.name"),
      desc: t("quota.desc"),
      build: (row) => {
        const text = quotaText(this.plugin.runner?.status().quota);
        if (text === null) {
          row.settingEl.hide();
          return;
        }
        row.setName(t("quota.name")).setDesc(text);
      },
    };
  }

  /** Итог последней проверки хранилища на сервере (`selfsync verify`). */
  private storageRow(): Row {
    return {
      name: t("storage.name"),
      desc: t("storage.desc"),
      build: (row) => {
        const text = storageText(this.plugin.runner?.status().storage);
        if (text === null) {
          row.settingEl.hide();
          return;
        }
        row.setName(t("storage.name")).setDesc(text);
      },
    };
  }

  /** Пресеты каталога настроек и итоговые правила. */
  private configSyncRows(): Row[] {
    const s = this.plugin.settings;
    const rows: Row[] = PRESETS.map((p: Preset) => ({
      name: t(`configSync.${p}`),
      desc: t(`configSync.${p}Desc`),
      build: (row: Setting) =>
        row
          .setName(t(`configSync.${p}`))
          .setDesc(t(`configSync.${p}Desc`))
          .addToggle((x) =>
            x.setValue(s.configSync[p]).onChange(async (v) => {
              s.configSync[p] = v;
              await this.plugin.saveSettings({ reconfigure: true });
              this.refresh();
            }),
          ),
    }));
    rows.push({
      name: t("configSync.rules"),
      desc: t("configSync.rulesDesc"),
      build: (row) => {
        row.setName(t("configSync.rules")).setDesc(t("configSync.rulesDesc"));
        // Раскрываемый блок: что на самом деле получает синк.
        const el = (row as { descEl?: HTMLElement }).descEl;
        if (!el?.createEl) return;
        const details = el.createEl("details");
        details.createEl("summary", { text: t("configSync.rulesShow") });
        details.createEl("pre", { text: buildRules(this.app.vault.configDir, s.configSync, s.excludes).join("\n") });
      },
    });
    return rows;
  }

  private encryptionRow(): Row {
    return {
      name: t("encryption.enable"),
      desc: t("encryption.off"),
      build: (row) => {
        const runner = this.plugin.runner;
        if (!runner) return;
        const st = runner.status();
        // Ключ на сервере есть, а у этого устройства его нет: индекс ещё в открытом
        // режиме (`encrypted` ложно), но предлагать «Включить» здесь — неправда.
        if (st.state === "needPassword") {
          row.setName("").setDesc(t("encryption.locked"));
          row.addButton((b) => b.setButtonText(t("encryption.unlock")).setCta().onClick(() => this.plugin.askPassword()));
          return;
        }
        if (st.reason === "encryption_started") {
          row.setName("").setDesc(t("encryption.starting"));
          return;
        }
        const encrypted = st.encrypted;
        row.setName("").setDesc(encrypted ? t("encryption.on") : t("encryption.off"));
        if (this.readOnly()) return;
        if (!encrypted) {
          row.addButton((b) =>
            b
              .setButtonText(t("encryption.enable"))
              .setWarning()
              .onClick(async () => {
                if (!(await this.confirmOldDevices(RECOVERY_MIN))) return;
                new PasswordModal(
                  this.app,
                  { title: t("encryption.enable"), desc: t("encryption.enableDesc"), submit: t("encryption.enable"), showStrength: true },
                  (r) => {
                    // Фраза восстановления создаётся сразу и приходит уведомлением.
                    runner.send({ type: "enableEncryption", password: r.password, remember: r.remember, recovery: true });
                    runner.send({ type: "syncNow" });
                  },
                ).open();
              }),
          );
        } else {
          row.addButton((b) =>
            b.setButtonText(t("encryption.change")).onClick(() =>
              new ChangePasswordModal(this.app, (old, next) => runner.send({ type: "changePassword", old, new: next })).open(),
            ),
          );
          row.addButton((b) => b.setButtonText(t("rekey.button")).setWarning().onClick(() => void this.rekey()));
        }
      },
    };
  }

  /** Фраза восстановления: есть/нет, создать (новую) или удалить — через пароль. */
  private recoveryRow(): Row {
    return {
      name: t("recovery.title"),
      build: (row) => {
        const runner = this.plugin.runner;
        if (!runner || !runner.status().encrypted || runner.status().state === "needPassword" || this.readOnly()) {
          row.settingEl.hide();
          return;
        }
        const has = runner.status().recovery;
        row.setName(t("recovery.status", { state: has ? t("recovery.on") : t("recovery.off") })).setDesc(t("recovery.createDesc"));
        const ask = (desc: string, submit: string, then: (password: string) => Promise<void>): void =>
          new PasswordModal(this.app, { title: t("recovery.title"), desc, submit, showStrength: false }, (r) => void then(r.password)).open();
        row.addButton((b) =>
          b
            .setButtonText(has ? t("recovery.createNew") : t("recovery.create"))
            .setCta()
            .onClick(async () => {
              // Новая фраза старым устройствам не мешает: формат записи тот же.
              if (!has && !(await this.confirmOldDevices(RECOVERY_MIN))) return;
              ask(t("recovery.createDesc"), has ? t("recovery.createNew") : t("recovery.create"), async (password) => {
                const r = await runner.command({ type: "createRecovery", password });
                if (r.type === "recoveryPhrase") new PhraseModal(this.app, r.words).open();
                else new Notice(r.type === "error" ? errorText(r) : r.type);
                this.refresh();
              });
            }),
        );
        if (has) {
          row.addButton((b) =>
            b
              .setButtonText(t("recovery.remove"))
              .setWarning()
              .onClick(() =>
                ask(t("recovery.removeDesc"), t("recovery.remove"), async (password) => {
                  const r = await runner.command({ type: "removeRecovery", password });
                  new Notice(r.type === "ok" ? t("recovery.removed") : r.type === "error" ? errorText(r) : r.type);
                  this.refresh();
                }),
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
        const readOnly = this.readOnly();
        row.addText((x) => {
          x.inputEl.type = "number";
          x.setDisabled(true);
          void runner.command({ type: "getRetention" }).then((r) => {
            if (r.type !== "retention") return;
            x.setValue(String(r.days)).setDisabled(readOnly);
          });
          x.onChange(async (raw) => {
            const days = Math.round(Number(raw));
            if (!Number.isFinite(days) || days < 1 || days > 3650) return;
            const r = await runner.command({ type: "setRetention", days });
            if (r.type === "error") new Notice(errorText(r));
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
        if (this.readOnly()) {
          row.settingEl.hide();
          return;
        }
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
                const readOnly = this.joinReadOnly;
                const r = await runner.command({ type: "createJoin", name: name || t("join.namePlaceholder"), readOnly });
                if (r.type === "join") new JoinModal(this.app, { ...r, readOnly }).open();
                else new Notice(r.type === "error" ? errorText(r) : r.type);
              }),
          );
      },
    };
  }

  /** «Только чтение» для следующей ссылки подключения. */
  private joinReadOnlyRow(): Row {
    return {
      name: t("join.readOnly"),
      desc: t("join.readOnlyDesc"),
      build: (row) => {
        if (!this.plugin.runner || this.readOnly()) {
          row.settingEl.hide();
          return;
        }
        // Сервер 0.1.x молча выдал бы по такой ссылке полный доступ.
        const minor = this.plugin.runner.status().serverMinor;
        if (minor != null && minor < readOnlyMinor()) {
          this.joinReadOnly = false;
          row
            .setName(t("join.readOnly"))
            .setDesc(t("join.readOnlyServerOutdated"))
            .addToggle((x) => x.setValue(false).setDisabled(true));
          return;
        }
        row
          .setName(t("join.readOnly"))
          .setDesc(t("join.readOnlyDesc"))
          .addToggle((x) => x.setValue(this.joinReadOnly).onChange((v) => (this.joinReadOnly = v)));
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

  /** Смена ключа шифрования: последствия, старые устройства, пароль. */
  private async rekey(): Promise<void> {
    const runner = this.plugin.runner;
    if (!runner) return;
    if (!(await confirm(this.app, t("rekey.confirm"), t("rekey.button")))) return;
    if (!(await this.confirmOldDevices(REKEY_MIN))) return;
    new PasswordModal(this.app, { title: t("rekey.title"), desc: t("rekey.desc"), submit: t("rekey.button"), showStrength: false }, (r) => {
      void runner.command({ type: "rekey", password: r.password, remember: r.remember }).then((res) => {
        new Notice(res.type === "error" ? errorText(res) : t("rekey.started"));
      });
    }).open();
  }

  /** Устройства старее `min` (или с неизвестной версией) — для предупреждения. Без
   *  сети список пуст: не мешать действию из-за того, что проверить не удалось. */
  private async oldDevices(min: string): Promise<string[]> {
    const r = await this.plugin.runner?.command({ type: "devices" });
    if (r?.type !== "devices") return [];
    return r.devices
      .filter((d) => !d.revoked && !d.current && clientOlderThan(d.client ?? "", min))
      .map((d) => `${d.name} (${d.client || t("devices.clientUnknown")})`);
  }

  /** Есть старые устройства — спросить; `true` — продолжать. */
  private async confirmOldDevices(min: string): Promise<boolean> {
    const old = await this.oldDevices(min);
    if (old.length === 0) return true;
    return confirm(this.app, t("devices.oldWarning", { min, names: old.join(", ") }), t("common.continue"));
  }

  private deviceRow(list: HTMLElement, d: DeviceView): void {
    const runner = this.plugin.runner;
    if (!runner) return;
    const seen = d.lastSeen > 0 ? new Date(d.lastSeen).toLocaleString() : "—";
    const row = new Setting(list)
      .setName(d.current ? `${d.name} (${t("devices.current")})` : d.name)
      .setDesc(
        [
          d.revoked ? t("devices.revoked") : t("devices.lastSeen", { when: seen }),
          d.readOnly ? t("devices.readOnly") : "",
          d.client ? d.client : t("devices.clientUnknown"),
        ]
          .filter((x) => x)
          .join(" · "),
      );
    if (d.revoked) row.settingEl.addClass("selfsync-device-revoked");
    if (d.current || d.revoked || this.readOnly()) return;
    row.addButton((b) =>
      b
        .setButtonText(t("devices.revoke"))
        .setWarning()
        .onClick(async () => {
          if (!(await confirm(this.app, t("devices.revokeConfirm", { name: d.name }), t("devices.revoke")))) return;
          const res = await runner.command({ type: "revokeDevice", id: d.id });
          if (res.type === "error") new Notice(errorText(res));
          this.refresh();
          // Отозванное устройство знает ключ шифрования: предложить его сменить.
          if (res.type !== "error" && runner.status().encrypted) {
            if (await confirm(this.app, t("rekey.afterRevoke", { name: d.name }), t("rekey.button"))) await this.rekey();
          }
        }),
    );
  }

  /** Файлы, не скачанные из-за лимита: окно со списком и «Скачать». */
  private skippedRow(): Row {
    return {
      name: t("skipped.title"),
      desc: t("skipped.desc"),
      build: (row) => {
        if (!this.plugin.runner) {
          row.settingEl.hide();
          return;
        }
        row
          .setName(t("skipped.title"))
          .setDesc(t("skipped.desc"))
          .addButton((b) => b.setButtonText(t("skipped.show")).onClick(() => new SkippedModal(this.app, this.plugin).open()));
      },
    };
  }

  private numberRow(name: string, get: () => number, min: number, max: number, set: (v: number) => void, desc?: string): Row {
    return {
      name,
      desc,
      build: (row) =>
        row
          .setName(name)
          .setDesc(desc ?? "")
          .addText((x) => {
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
