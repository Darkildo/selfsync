// История ревизий файла на сервере; любую живую ревизию можно вернуть — она станет
// новой версией (старые остаются в истории).

import { type App, Modal, Notice, Setting } from "obsidian";

import { formatBytes, t } from "../i18n.ts";
import type SelfsyncPlugin from "../main.ts";

export class HistoryModal extends Modal {
  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
    private path: string,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle(t("history.title", { path: this.path }));
    void this.render();
  }

  private async render(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    const runner = this.plugin.runner;
    if (!runner) return;
    const r = await runner.command({ type: "history", path: this.path });
    if (r.type !== "history") {
      contentEl.createEl("p", { text: r.type === "error" ? r.message : r.type });
      return;
    }
    if (r.revisions.length === 0) {
      contentEl.createEl("p", { text: t("history.none") });
      return;
    }
    r.revisions.forEach((rev, i) => {
      const what = rev.deleted ? t("history.deleted") : rev.renamedFrom ? t("history.renamed", { from: rev.renamedFrom }) : formatBytes(rev.size);
      const s = new Setting(contentEl)
        .setName(t("history.rev", { rev: rev.rev, when: new Date(rev.mtime || 0).toLocaleString() }))
        .setDesc(`${what} · ${t("history.device", { id: rev.device })}${i === 0 ? ` · ${t("history.current")}` : ""}`);
      if (i > 0 && !rev.deleted) {
        s.addButton((b) =>
          b.setButtonText(t("history.restore")).onClick(async () => {
            const res = await runner.command({ type: "restoreRevision", path: this.path, rev: rev.rev });
            new Notice(res.type === "error" ? res.message : t("history.restored", { rev: rev.rev }));
            await this.render();
          }),
        );
      }
    });
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
