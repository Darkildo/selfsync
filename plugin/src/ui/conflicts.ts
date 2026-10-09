// Нерешённые конфликты (8.4): серверная версия на месте, своя — копией рядом.
// Пользователь выбирает: оставить обе, свою или серверную.

import { type App, Modal, Notice, Setting } from "obsidian";

import { t } from "../i18n.ts";
import type NotesyncPlugin from "../main.ts";
import type { ConflictChoice, ConflictRecord } from "../types.ts";

export class ConflictsModal extends Modal {
  constructor(
    app: App,
    private plugin: NotesyncPlugin,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle(t("conflicts.title"));
    void this.render();
  }

  private async render(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    const runner = this.plugin.runner;
    if (!runner) return;
    const r = await runner.command({ type: "conflicts" });
    if (r.type !== "conflicts") {
      contentEl.createEl("p", { text: r.type === "error" ? r.message : r.type });
      return;
    }
    if (r.items.length === 0) {
      contentEl.createEl("p", { text: t("conflicts.none") });
      return;
    }
    contentEl.createEl("p", { cls: "setting-item-description", text: t("conflicts.desc") });
    for (const c of r.items) this.item(c);
  }

  private item(c: ConflictRecord): void {
    const s = new Setting(this.contentEl)
      .setName(c.path)
      .setDesc(t("conflicts.copy", { copy: c.copy, time: new Date(c.at).toLocaleString() }));
    const open = (path: string) => void this.app.workspace.openLinkText(path, "", "tab");
    s.addExtraButton((b) => b.setIcon("file-text").setTooltip(t("conflicts.openServer")).onClick(() => open(c.path)));
    s.addExtraButton((b) => b.setIcon("files").setTooltip(t("conflicts.openMine")).onClick(() => open(c.copy)));
    const choose = (choice: ConflictChoice) => async () => {
      this.plugin.runner?.send({ type: "resolve", id: c.id, choice });
      await this.plugin.runner?.idle();
      new Notice(t("conflicts.resolved", { path: c.path }));
      await this.render();
    };
    s.addButton((b) => b.setButtonText(t("conflicts.keepBoth")).onClick(choose("keepBoth")));
    s.addButton((b) => b.setButtonText(t("conflicts.keepMine")).onClick(choose("keepMine")));
    s.addButton((b) => b.setButtonText(t("conflicts.keepServer")).onClick(choose("keepServer")));
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
