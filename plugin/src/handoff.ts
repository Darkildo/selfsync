// Передача ключа по коду (протокол 1.4): ссылка для QR и опрос шагов ядра. Вся
// криптография — в ядре; здесь только расписание и формат для интерфейса.

import type { HandoffView, UiResult } from "./types.ts";

/** Опрос шагов обмена. */
export const HANDOFF_POLL_MS = 1500;
/** Самая длинная пауза опроса, пока сервер недоступен. */
export const HANDOFF_BACKOFF_MAX_MS = 15_000;
/** Сколько новое устройство ждёт ключ (столько же сервер хранит обмен). */
export const HANDOFF_WAIT_MS = 10 * 60 * 1000;

/**
 * Ссылка для QR с секретом обмена `m` и префиксом обязательства `h`. Строит её
 * устройство-источник: через сервер она не должна проходить (страница
 * `/join/{code}` их не знает), поэтому QR ведёт сразу в Obsidian.
 */
export function connectLink(server: string, code: string, secret: string, check: string): string {
  const q = new URLSearchParams({ server, code, m: secret, h: check });
  return `obsidian://selfsync-connect?${q.toString()}`;
}

/** Шесть цифр группами для глаз: `042 917`. */
export function formatSas(sas: string): string {
  return sas.length === 6 ? `${sas.slice(0, 3)} ${sas.slice(3)}` : sas;
}

export function isHandoff(r: UiResult): r is HandoffView {
  return r.type === "handoff";
}

export interface PollOptions {
  /** Один шаг; ответ не `handoff` — ошибка, опрос заканчивается. */
  step(): Promise<UiResult>;
  /** Каждый ответ (и шаг, и ошибка). */
  onResult(r: UiResult): void;
  /** Остановить опрос (окно закрыто). */
  stopped(): boolean;
  deadline: number;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
  interval?: number;
}

/** Опрашивать до `done`/`sent`/`rejected`, ошибки, остановки или срока. Без связи
 *  (`offline`) пауза растёт вдвое до 15 с. Возвращает последний ответ (`null` —
 *  остановлено или срок вышел). */
export async function pollHandoff(o: PollOptions): Promise<UiResult | null> {
  const now = o.now ?? Date.now;
  const sleep = o.sleep ?? ((ms: number) => new Promise<void>((r) => window.setTimeout(r, ms)));
  const base = o.interval ?? HANDOFF_POLL_MS;
  let wait = base;
  while (!o.stopped() && now() < o.deadline) {
    const r = await o.step();
    if (o.stopped()) return null;
    o.onResult(r);
    if (!isHandoff(r) || r.step === "done" || r.step === "sent" || r.step === "rejected") return r;
    wait = r.offline ? Math.min(wait * 2, Math.max(HANDOFF_BACKOFF_MAX_MS, base)) : base;
    await sleep(wait);
  }
  return null;
}
