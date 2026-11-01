// «Подключить новое устройство»: одноразовая ссылка и её QR-код. Телефон открывает
// ссылку камерой — страница сервера ведёт в obsidian://selfsync-connect. Если ключ
// шифрования раскрыт здесь, QR ведёт сразу в Obsidian с секретом обмена, и окно
// передаёт ключ новому устройству (протокол 1.4) — после согласия пользователя.

import { type App, Modal, Setting } from "obsidian";

import { qrModules } from "../../pkg/selfsync_wasm.js";
import type { Runner } from "../engine.ts";
import { connectLink, formatSas, HANDOFF_WAIT_MS, pollHandoff } from "../handoff.ts";
import { t } from "../i18n.ts";
import type { UiResult } from "../types.ts";

const SVG = "http://www.w3.org/2000/svg";
const QUIET = 4;

/** SVG из матрицы модулей `[ширина, модули…]` (как отдаёт ядро). */
export function qrSvg(doc: Document, modules: Uint8Array): SVGSVGElement {
  const width = modules[0] ?? 0;
  const size = width + QUIET * 2;
  let d = "";
  for (let y = 0; y < width; y++) {
    for (let x = 0; x < width; x++) {
      if (modules[1 + y * width + x]) d += `M${x + QUIET} ${y + QUIET}h1v1h-1z`;
    }
  }
  const svg = doc.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", `0 0 ${size} ${size}`);
  svg.setAttribute("shape-rendering", "crispEdges");
  svg.classList.add("selfsync-qr");
  const bg = doc.createElementNS(SVG, "rect");
  bg.setAttribute("width", String(size));
  bg.setAttribute("height", String(size));
  bg.setAttribute("fill", "#fff");
  const path = doc.createElementNS(SVG, "path");
  path.setAttribute("d", d);
  path.setAttribute("fill", "#000");
  svg.append(bg, path);
  return svg;
}

/** Передача ключа из этого окна: движок и адрес сервера для ссылки. */
export interface JoinHandoff {
  runner: Runner;
  server: string;
}

export class JoinModal extends Modal {
  private closed = false;
  /** Цифры, на которых пользователь согласился отдать ключ. */
  private confirm: string | null = null;

  constructor(
    app: App,
    private join: { url: string; code: string; expiresAt: number; readOnly?: boolean; secret?: string | null },
    private handoff?: JoinHandoff,
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    this.setTitle(t("join.title"));
    contentEl.createEl("p", { text: t("join.desc") });
    if (this.join.readOnly) contentEl.createEl("p", { text: `${t("join.readOnly")}: ${t("join.readOnlyDesc")}` });
    const h = this.handoff;
    const secret = this.join.secret;
    // Секрет — только в QR: ссылку для копирования пересылают, и с секретом
    // она отдавала бы ключ без сверки цифр.
    const qr = h && secret ? connectLink(h.server, this.join.code, secret) : this.join.url;
    contentEl.appendChild(qrSvg(document, qrModules(qr)));
    if (h && secret) contentEl.createEl("p", { text: t("handoff.offer"), cls: "setting-item-description" });
    new Setting(contentEl)
      .setName(this.join.url)
      .setDesc(t("join.code", { code: this.join.code, until: new Date(this.join.expiresAt).toLocaleTimeString() }))
      .addButton((b) => b.setButtonText(t("join.copy")).onClick(() => void navigator.clipboard.writeText(this.join.url)));
    if (h && secret) {
      const status = contentEl.createDiv({ cls: "selfsync-handoff" });
      void pollHandoff({
        step: () => h.runner.command({ type: "joinHandoff", code: this.join.code, confirm: this.confirm }),
        onResult: (r) => this.render(status, r),
        stopped: () => this.closed,
        deadline: this.join.expiresAt + HANDOFF_WAIT_MS,
      });
    }
  }

  private render(el: HTMLElement, r: UiResult): void {
    el.empty();
    if (r.type === "error") {
      el.createEl("p", { text: t("handoff.failed", { message: r.message }) });
      return;
    }
    if (r.type !== "handoff") return;
    if (r.step === "waiting") {
      el.createEl("p", { text: t("handoff.waiting") });
      return;
    }
    if (r.step === "done") {
      el.createEl("p", { text: t("handoff.sent", { name: r.name }) });
      return;
    }
    const sas = r.sas ?? "";
    if (this.confirm === sas) {
      el.createEl("p", { text: t("handoff.sending") });
      return;
    }
    el.createEl("p", { text: t(r.qr ? "handoff.confirmQr" : "handoff.confirmSas", { name: r.name }) });
    if (!r.qr) el.createEl("p", { text: formatSas(sas), cls: "selfsync-sas" });
    new Setting(el)
      .addButton((b) => b.setButtonText(t("handoff.decline")).onClick(() => this.close()))
      .addButton((b) =>
        b
          .setButtonText(t("handoff.send"))
          .setCta()
          .onClick(() => {
            this.confirm = sas;
            el.empty();
            el.createEl("p", { text: t("handoff.sending") });
          }),
      );
  }

  override onClose(): void {
    this.closed = true;
    if (this.handoff && this.join.secret) void this.handoff.runner.command({ type: "cancelHandoff", code: this.join.code });
    this.contentEl.empty();
  }
}
