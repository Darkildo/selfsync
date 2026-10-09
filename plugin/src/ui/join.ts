// «Подключить новое устройство»: одноразовая ссылка и её QR-код. Телефон открывает
// ссылку камерой — страница сервера ведёт в obsidian://selfsync-connect.

import { type App, Modal, Setting } from "obsidian";

import { qrModules } from "../../pkg/selfsync_wasm.js";
import { t } from "../i18n.ts";

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

export class JoinModal extends Modal {
  constructor(
    app: App,
    private join: { url: string; code: string; expiresAt: number },
  ) {
    super(app);
  }

  override onOpen(): void {
    const { contentEl } = this;
    this.setTitle(t("join.title"));
    contentEl.createEl("p", { text: t("join.desc") });
    contentEl.appendChild(qrSvg(document, qrModules(this.join.url)));
    new Setting(contentEl)
      .setName(this.join.url)
      .setDesc(t("join.code", { code: this.join.code, until: new Date(this.join.expiresAt).toLocaleTimeString() }))
      .addButton((b) => b.setButtonText(t("join.copy")).onClick(() => void navigator.clipboard.writeText(this.join.url)));
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
