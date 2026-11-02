// Подключение по одноразовому коду: из ссылки obsidian://selfsync-connect или
// вручную. Код обменивается на токен устройства; если устройство, где открыт код,
// передаёт ключ шифрования, окно ждёт его (протокол 1.4) — пароль не нужен. Цифры
// обмена показываются всегда; без QR (`h=`) ключ примется только после того, как
// пользователь сверит их здесь.

import { type App, Modal, Notice, Setting } from "obsidian";

import { formatSas } from "../handoff.ts";
import { t } from "../i18n.ts";
import type SelfsyncPlugin from "../main.ts";
import { normalizeServer } from "../settings.ts";
import type { DiagCheck, UiResult } from "../types.ts";
import { DiagnoseModal } from "./diagnose.ts";

export class ConnectModal extends Modal {
  private closed = false;
  private stopWaiting = false;
  /** Цифры, которые пользователь сверил на этом устройстве. */
  private confirmed: string | null = null;

  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
    private server: string,
    private code: string,
    private onDone?: () => void,
    /** `m=` из ссылки QR (секрет обмена ключом). */
    private secret?: string,
    /** `h=` из ссылки QR (префикс обязательства источника). */
    private check?: string,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.modalEl.addClass("selfsync-actions");
    const { contentEl } = this;
    let device = this.plugin.settings.deviceName;
    this.setTitle(t("connect.title"));
    const form = contentEl.createDiv();
    const status = contentEl.createDiv({ cls: "selfsync-handoff" });
    new Setting(form).setName(t("connect.server")).addText((x) => {
      x.inputEl.dir = "ltr";
      x.setValue(this.server).onChange((v) => (this.server = v));
    });
    new Setting(form).setName(t("connect.code")).addText((x) => {
      x.inputEl.dir = "ltr";
      x.setValue(this.code).onChange((v) => {
        this.code = v;
        // Код ввели заново — секрет из прежней ссылки к нему не относится.
        this.secret = undefined;
        this.check = undefined;
      });
    });
    new Setting(form).setName(t("connect.device")).addText((x) => x.setValue(device).onChange((v) => (device = v)));
    new Setting(form)
      .addButton((b) => b.setButtonText(t("common.cancel")).onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(t("connect.submit"))
          .setCta()
          .onClick(async () => {
            b.setDisabled(true);
            const server = normalizeServer(this.server);
            const err = await this.plugin.redeem(server, this.code.trim(), device.trim() || this.plugin.settings.deviceName, {
              secret: this.secret,
              check: this.check,
              onResult: (r) => {
                form.hide();
                this.renderReceive(status, r);
              },
              confirmed: () => this.confirmed,
              stopped: () => this.closed || this.stopWaiting,
            });
            b.setDisabled(false);
            if (err) {
              new Notice(t("connect.failed", { message: err.message }));
              // Сервер не ответил: показать, что именно не так с адресом.
              if (err.offline) {
                const run = (): Promise<DiagCheck[]> => this.plugin.diagnoseServer(server);
                new DiagnoseModal(this.app, run).open();
              }
              return;
            }
            new Notice(t("connect.done", { vault: this.plugin.settings.vault }));
            this.close();
            this.onDone?.();
            void this.plugin.checkAfterConnect();
          }),
      );
  }

  /** Ждём ключ шифрования: что сказать и что показать. */
  private renderReceive(el: HTMLElement, r: UiResult): void {
    el.empty();
    if (r.type === "error") {
      el.createEl("p", { text: t("handoff.receiveFailed", { message: r.message }) });
      return;
    }
    if (r.type !== "handoff") return;
    if (r.step === "done") {
      el.createEl("p", { text: t("handoff.received") });
      return;
    }
    if (r.offline) el.createEl("p", { text: t("handoff.offline"), cls: "setting-item-description" });
    const stop = (): void => {
      this.stopWaiting = true;
      el.empty();
    };
    if (r.step === "confirm" && r.sas) {
      el.createEl("p", { text: formatSas(r.sas), cls: "selfsync-sas" });
      if (r.qr) {
        // По QR сверять не обязательно; если источник всё же их показал — сервер
        // мог убрать MAC: сверить.
        el.createEl("p", { text: t("handoff.digitsQr") });
      } else if (this.confirmed !== r.sas) {
        el.createEl("p", { text: t("handoff.compare") });
        const sas = r.sas;
        new Setting(el)
          .addButton((b) =>
            b.setButtonText(t("handoff.mismatch")).onClick(() => {
              stop();
              el.createEl("p", { text: t("handoff.mismatchDone") });
            }),
          )
          .addButton((b) =>
            b
              .setButtonText(t("handoff.match"))
              .setCta()
              .onClick(() => {
                this.confirmed = sas;
                this.renderReceive(el, r);
              }),
          );
        return;
      } else {
        el.createEl("p", { text: t("handoff.approveThere") });
      }
    } else {
      el.createEl("p", { text: t("handoff.receiving") });
    }
    new Setting(el).addButton((b) => b.setButtonText(t("handoff.usePassword")).onClick(stop));
  }

  override onClose(): void {
    this.closed = true;
    this.contentEl.empty();
  }
}
