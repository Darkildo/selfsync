// Подтверждение опасного действия: окно Obsidian вместо window.confirm, который на
// мобильных выглядит чужеродно, а в каталоге плагинов считается ошибкой.

import { type App, Modal, Setting } from "obsidian";

import { t } from "../i18n.ts";

class ConfirmModal extends Modal {
  private confirmed = false;

  constructor(
    app: App,
    private text: string,
    private action: string,
    private done: (ok: boolean) => void,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.contentEl.createEl("p", { text: this.text });
    new Setting(this.contentEl)
      .addButton((b) => b.setButtonText(t("common.cancel")).onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(this.action)
          .setWarning()
          .onClick(() => {
            this.confirmed = true;
            this.close();
          }),
      );
  }

  override onClose(): void {
    this.contentEl.empty();
    this.done(this.confirmed);
  }
}

export function confirm(app: App, text: string, action: string): Promise<boolean> {
  return new Promise((resolve) => new ConfirmModal(app, text, action, resolve).open());
}
