// Свои иконки статус-бара: общий знак — кольцо из двух дуг с разрывами (наш
// «круг синхронизации»), внутри — глиф состояния. Рисуем в сетке 24×24, как Lucide,
// чтобы толщина линии совпадала со встроенными иконками; addIcon ждёт 100×100.

import { addIcon } from "obsidian";

/** Вид статуса: состояние ядра плюс «не настроено» и «только чтение». */
export type StatusKind = "none" | "readOnly" | "idle" | "syncing" | "offline" | "error" | "needPassword" | "blocked";

const RING = "M19.79 7.5A9 9 0 0 1 7.5 19.79M4.21 16.5A9 9 0 0 1 16.5 4.21";

const GLYPHS: Record<StatusKind, string> = {
  none: "",
  idle: "M8.6 12.2l2.3 2.3 4.6-4.7",
  syncing: "M12 12h.01",
  offline: "M8.5 12h.01M12 12h.01M15.5 12h.01",
  error: "M12 8.5v3.8M12 15.5h.01",
  needPassword: "M12 12.2v3M13.7 10.4a1.7 1.7 0 1 1-3.4 0 1.7 1.7 0 0 1 3.4 0z",
  blocked: "M9.2 14.8l5.6-5.6",
  readOnly: "M7.6 12s1.6-2.8 4.4-2.8 4.4 2.8 4.4 2.8-1.6 2.8-4.4 2.8-4.4-2.8-4.4-2.8zM12 12h.01",
};

export function statusIcon(kind: StatusKind): string {
  return `selfsync-${kind}`;
}

export function registerIcons(): void {
  for (const [kind, glyph] of Object.entries(GLYPHS) as [StatusKind, string][]) {
    const d = glyph ? `${RING}${glyph}` : RING;
    addIcon(
      statusIcon(kind),
      `<g transform="scale(4.1667)" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round"><path d="${d}"/></g>`,
    );
  }
}
