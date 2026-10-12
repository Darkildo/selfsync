// Проверка соединения по шагам: что не так и что сделать. Шаги выполняет ядро
// (команда diagnose), здесь — только вывод.

import { type App, Modal, Setting } from "obsidian";

import { diagText, t } from "../i18n.ts";
import type { DiagCheck } from "../types.ts";

const MARK: Record<DiagCheck["level"], string> = { ok: "✓", warn: "⚠", fail: "✗", skip: "–" };

/** Есть ли что показать человеку. */
export function hasProblems(checks: DiagCheck[]): boolean {
  return checks.some((c) => c.level === "warn" || c.level === "fail");
}

export class DiagnoseModal extends Modal {
  constructor(
    app: App,
    /** Проверка; `null` в ответе — проверить нечем (нет подключения). */
    private run: () => Promise<DiagCheck[] | null>,
    /** Уже готовый итог: показать его, не запуская проверку заново. */
    private checks: DiagCheck[] | null = null,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle(t("diag.title"));
    void this.render(this.checks);
  }

  private async render(ready: DiagCheck[] | null): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    let checks = ready;
    if (!checks) {
      contentEl.createEl("p", { text: t("diag.running") });
      checks = await this.run();
      contentEl.empty();
    }
    for (const c of checks ?? []) {
      new Setting(contentEl).setName(`${MARK[c.level]} ${t(`diag.step.${c.step}`)}`).setDesc(diagText(c));
    }
    new Setting(contentEl).addButton((b) => b.setButtonText(t("diag.again")).onClick(() => void this.render(null)));
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
