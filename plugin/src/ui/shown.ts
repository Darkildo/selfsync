// Открыта ли вкладка настроек. В Obsidian 1.13+ строки из getSettingDefinitions()
// рисуются не в containerEl вкладки, так что containerEl.isConnected всегда false.
// Судим по строкам, которые вкладка отрисовала последними: хоть одна в DOM — открыта.

export class ShownRows {
  private els = new Set<{ isConnected: boolean }>();

  add(el: { isConnected: boolean }): void {
    this.els.add(el);
  }

  /** Есть ли на экране хоть одна отрисованная строка. Убранные из DOM забываются. */
  any(): boolean {
    for (const el of this.els) {
      if (el.isConnected) return true;
      this.els.delete(el);
    }
    return false;
  }
}
