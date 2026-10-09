// Ввод пароля шифрования. Оценка стойкости — совет, а не запрет (раздел 10.4).

import { type App, Modal, Setting } from "obsidian";

import { passwordStrength } from "../../pkg/selfsync_wasm.js";
import { t } from "../i18n.ts";

/** Ниже этого — предупреждение о слабом пароле. */
const WEAK_BITS = 50;

export interface PasswordResult {
  password: string;
  remember: boolean;
}

export class PasswordModal extends Modal {
  private password = "";
  private remember = true;

  constructor(
    app: App,
    private opts: { title: string; desc: string; submit: string; showStrength: boolean },
    private onSubmit: (r: PasswordResult) => void,
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    this.setTitle(this.opts.title);
    contentEl.createEl("p", { text: this.opts.desc });
    const hint = contentEl.createEl("p", { cls: "setting-item-description" });
    new Setting(contentEl).setName(t("password.password")).addText((x) => {
      x.inputEl.type = "password";
      x.onChange((v) => {
        this.password = v;
        if (!this.opts.showStrength) return;
        const bits = passwordStrength(v);
        hint.setText(`${t("password.strength", { bits })}${bits < WEAK_BITS ? ` — ${t("password.weak")}` : ""}`);
      });
      x.inputEl.addEventListener("keydown", (e) => {
        if (e.key === "Enter") this.submit();
      });
    });
    new Setting(contentEl)
      .setName(t("password.remember"))
      .setDesc(t("password.rememberDesc"))
      .addToggle((x) => x.setValue(this.remember).onChange((v) => (this.remember = v)));
    new Setting(contentEl)
      .addButton((b) => b.setButtonText(t("common.cancel")).onClick(() => this.close()))
      .addButton((b) => b.setButtonText(this.opts.submit).setCta().onClick(() => this.submit()));
  }

  private submit(): void {
    if (this.password === "") return;
    this.close();
    this.onSubmit({ password: this.password, remember: this.remember });
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}

/** Смена пароля: мастер-ключ тот же, перешифровывать ничего не нужно. */
export class ChangePasswordModal extends Modal {
  private old = "";
  private next = "";

  constructor(
    app: App,
    private onSubmit: (old: string, next: string) => void,
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    this.setTitle(t("encryption.changeTitle"));
    const hint = contentEl.createEl("p", { cls: "setting-item-description" });
    new Setting(contentEl).setName(t("encryption.oldPassword")).addText((x) => {
      x.inputEl.type = "password";
      x.onChange((v) => (this.old = v));
    });
    new Setting(contentEl).setName(t("encryption.newPassword")).addText((x) => {
      x.inputEl.type = "password";
      x.onChange((v) => {
        this.next = v;
        const bits = passwordStrength(v);
        hint.setText(`${t("password.strength", { bits })}${bits < WEAK_BITS ? ` — ${t("password.weak")}` : ""}`);
      });
    });
    new Setting(contentEl)
      .addButton((b) => b.setButtonText(t("common.cancel")).onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(t("encryption.change"))
          .setCta()
          .onClick(() => {
            if (this.old === "" || this.next === "") return;
            this.close();
            this.onSubmit(this.old, this.next);
          }),
      );
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
