// Фраза восстановления: показ один раз (при включении шифрования или по кнопке в
// настройках) и вход по фразе, если пароль забыт. Фраза не пишется ни в журнал
// плагина, ни в localStorage, ни в настройки.

import { type App, Modal, Notice, Setting } from "obsidian";

import { checkPhrase, passwordStrength } from "../../pkg/selfsync_wasm.js";
import { t } from "../i18n.ts";

const WEAK_BITS = 50;

/** Текст ошибки проверки фразы (`checkPhrase` ядра) или `null`. */
export function phraseError(phrase: string): string | null {
  const r = checkPhrase(phrase);
  if (r === undefined) return null;
  const [kind, n] = r.split(":");
  if (kind === "count") return t("recovery.errCount", { n: n ?? "0" });
  if (kind === "word") return t("recovery.errWord", { n: n ?? "?" });
  return t("recovery.errChecksum");
}

/** 24 слова: пронумерованы, моноширинно; закрыть можно, только отметив «записал». */
export class PhraseModal extends Modal {
  private confirmed = false;

  constructor(
    app: App,
    private words: string[],
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    this.setTitle(t("recovery.title"));
    contentEl.createEl("p", { text: t("recovery.showDesc") });
    const grid = contentEl.createDiv({ cls: "selfsync-phrase" });
    this.words.forEach((w, i) => grid.createDiv({ text: `${i + 1}. ${w}` }));
    new Setting(contentEl).addButton((b) =>
      b.setButtonText(t("recovery.copy")).onClick(() => {
        // Только запись в буфер; читать буфер плагин не умеет и не будет.
        void navigator.clipboard.writeText(this.words.join(" "));
        new Notice(t("recovery.copied"));
      }),
    );
    new Setting(contentEl).setName(t("recovery.written")).addToggle((x) => x.onChange((v) => (this.confirmed = v)));
    new Setting(contentEl).addButton((b) =>
      b
        .setButtonText(t("recovery.done"))
        .setCta()
        .onClick(() => this.close()),
    );
  }

  override close(): void {
    // Esc и клик мимо окна тоже сюда: без отметки фраза пропала бы навсегда.
    if (!this.confirmed) {
      new Notice(t("recovery.confirmFirst"));
      return;
    }
    super.close();
  }

  override onClose(): void {
    this.words = [];
    this.contentEl.empty();
  }
}

export interface RecoverResult {
  phrase: string;
  newPassword: string;
  remember: boolean;
}

/** Забыли пароль: фраза и новый пароль. Фраза проверяется до отправки. */
export class RecoverModal extends Modal {
  private phrase = "";
  private password = "";
  private remember = true;

  constructor(
    app: App,
    private onSubmit: (r: RecoverResult) => Promise<string | null>,
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    this.setTitle(t("recovery.recoverTitle"));
    contentEl.createEl("p", { text: t("recovery.recoverDesc") });
    const err = contentEl.createEl("p", { cls: "mod-warning" });
    new Setting(contentEl).setName(t("recovery.phrase")).addTextArea((x) => {
      x.inputEl.rows = 4;
      x.inputEl.addClass("selfsync-phrase-input");
      x.onChange((v) => {
        this.phrase = v;
        err.setText(v.trim() === "" ? "" : (phraseError(v) ?? ""));
      });
    });
    const hint = contentEl.createEl("p", { cls: "setting-item-description" });
    new Setting(contentEl).setName(t("encryption.newPassword")).addText((x) => {
      x.inputEl.type = "password";
      x.onChange((v) => {
        this.password = v;
        const bits = passwordStrength(v);
        hint.setText(`${t("password.strength", { bits })}${bits < WEAK_BITS ? ` — ${t("password.weak")}` : ""}`);
      });
    });
    new Setting(contentEl)
      .setName(t("password.remember"))
      .setDesc(t("password.rememberDesc"))
      .addToggle((x) => x.setValue(this.remember).onChange((v) => (this.remember = v)));
    new Setting(contentEl)
      .addButton((b) => b.setButtonText(t("common.cancel")).onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(t("recovery.recover"))
          .setCta()
          .onClick(async () => {
            const bad = phraseError(this.phrase);
            if (bad) return err.setText(bad);
            if (this.password === "") return err.setText(t("recovery.needPassword"));
            b.setDisabled(true);
            const failed = await this.onSubmit({ phrase: this.phrase, newPassword: this.password, remember: this.remember });
            b.setDisabled(false);
            if (failed) return err.setText(failed);
            this.close();
          }),
      );
  }

  override onClose(): void {
    this.phrase = "";
    this.password = "";
    this.contentEl.empty();
  }
}
