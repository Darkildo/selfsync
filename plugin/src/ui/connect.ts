// Подключение по одноразовому коду: из ссылки obsidian://selfsync-connect или
// вручную. Код обменивается на токен устройства.

import { type App, Modal, Notice, Setting } from "obsidian";

import { t } from "../i18n.ts";
import type SelfsyncPlugin from "../main.ts";
import { normalizeServer } from "../settings.ts";
import type { DiagCheck } from "../types.ts";
import { DiagnoseModal } from "./diagnose.ts";

export class ConnectModal extends Modal {
  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
    private server: string,
    private code: string,
    private onDone?: () => void,
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    let device = this.plugin.settings.deviceName;
    this.setTitle(t("connect.title"));
    new Setting(contentEl).setName(t("connect.server")).addText((x) => x.setValue(this.server).onChange((v) => (this.server = v)));
    new Setting(contentEl).setName(t("connect.code")).addText((x) => x.setValue(this.code).onChange((v) => (this.code = v)));
    new Setting(contentEl).setName(t("connect.device")).addText((x) => x.setValue(device).onChange((v) => (device = v)));
    new Setting(contentEl)
      .addButton((b) => b.setButtonText(t("common.cancel")).onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(t("connect.submit"))
          .setCta()
          .onClick(async () => {
            b.setDisabled(true);
            const server = normalizeServer(this.server);
            const err = await this.plugin.redeem(server, this.code.trim(), device.trim() || this.plugin.settings.deviceName);
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

  override onClose(): void {
    this.contentEl.empty();
  }
}
