// Подключение по одноразовому коду: из ссылки obsidian://notesync-connect или
// вручную. Код обменивается на токен устройства.

import { type App, Modal, Notice, Setting } from "obsidian";

import { t } from "../i18n.ts";
import type NotesyncPlugin from "../main.ts";
import { normalizeServer } from "../settings.ts";

export class ConnectModal extends Modal {
  constructor(
    app: App,
    private plugin: NotesyncPlugin,
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
            const err = await this.plugin.redeem(normalizeServer(this.server), this.code.trim(), device.trim() || this.plugin.settings.deviceName);
            b.setDisabled(false);
            if (err) {
              new Notice(t("connect.failed", { message: err }));
              return;
            }
            new Notice(t("connect.done", { vault: this.plugin.settings.vault }));
            this.close();
            this.onDone?.();
          }),
      );
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
