// Файлы, не скачанные из-за лимита размера (облегчённое устройство): на сервере и
// других устройствах они есть; отсюда любой можно скачать.

import { type App, Modal, Notice, Setting } from "obsidian";

import { formatBytes, t } from "../i18n.ts";
import type SelfsyncPlugin from "../main.ts";

export class SkippedModal extends Modal {
  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle(t("skipped.title"));
    void this.render();
  }

  private async render(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    const runner = this.plugin.runner;
    if (!runner) return;
    const r = await runner.command({ type: "skipped" });
    if (r.type !== "skipped") {
      contentEl.createEl("p", { text: r.type === "error" ? r.message : r.type });
      return;
    }
    if (r.files.length === 0) {
      contentEl.createEl("p", { text: t("skipped.none") });
      return;
    }
    for (const f of r.files) {
      new Setting(contentEl)
        .setName(f.path)
        .setDesc(formatBytes(f.size))
        .addButton((b) =>
          b.setButtonText(t("skipped.download")).onClick(async () => {
            b.setDisabled(true);
            const res = await runner.command({ type: "download", path: f.path });
            new Notice(res.type === "error" ? res.message : t("skipped.queued", { path: f.path }));
          }),
        );
    }
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
