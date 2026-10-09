// Корзина сервера: удалённое на любом устройстве хранится окно хранения; отсюда
// его можно вернуть или стереть окончательно.

import { type App, Modal, Notice, Setting } from "obsidian";

import { formatBytes, t } from "../i18n.ts";
import type SelfsyncPlugin from "../main.ts";
import type { DeletedView } from "../types.ts";

export class DeletedModal extends Modal {
  private selected = new Set<string>();

  constructor(
    app: App,
    private plugin: SelfsyncPlugin,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle(t("deleted.title"));
    void this.render();
  }

  private async render(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    this.selected.clear();
    const runner = this.plugin.runner;
    if (!runner) return;
    const r = await runner.command({ type: "listDeleted" });
    if (r.type !== "deleted") {
      contentEl.createEl("p", { text: r.type === "error" ? r.message : r.type });
      return;
    }
    if (r.items.length === 0) {
      contentEl.createEl("p", { text: t("deleted.none") });
      return;
    }
    const items = [...r.items].sort((a, b) => b.deletedAt - a.deletedAt);
    const actions = new Setting(contentEl);
    actions.addButton((b) => b.setButtonText(t("deleted.restore")).setCta().onClick(() => this.apply("restore")));
    actions.addButton((b) => b.setButtonText(t("deleted.purge")).setWarning().onClick(() => this.apply("purge")));
    for (const d of items) this.item(d);
  }

  private item(d: DeletedView): void {
    new Setting(this.contentEl)
      .setName(d.path)
      .setDesc(
        t("deleted.meta", {
          size: formatBytes(d.size),
          when: new Date(d.deletedAt).toLocaleString(),
          until: new Date(d.expiresAt).toLocaleDateString(),
        }),
      )
      .addToggle((x) =>
        x.onChange((v) => {
          if (v) this.selected.add(d.path);
          else this.selected.delete(d.path);
        }),
      );
  }

  private async apply(what: "restore" | "purge"): Promise<void> {
    const paths = [...this.selected];
    const runner = this.plugin.runner;
    if (paths.length === 0 || !runner) return;
    if (what === "purge" && !window.confirm(t("deleted.purgeConfirm", { n: paths.length }))) return;
    const r = await runner.command(what === "restore" ? { type: "restoreDeleted", paths } : { type: "purgeDeleted", paths });
    if (r.type === "error") new Notice(r.message);
    else if (r.type === "restored") new Notice(t("deleted.restored", { n: r.count }));
    await this.render();
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
