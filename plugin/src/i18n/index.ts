// Строки интерфейса. Язык — как у Obsidian (getLanguage()). Ядро присылает
// только коды и данные, весь текст формируется здесь. Словари — en.ts (эталон),
// ru.ts и переводы из реестра locales.ts.

import type { DiagCheck, Notice, QuotaStatus, StorageHealth, SyncStatus } from "../types.ts";
import { type Dict, en, type Key } from "./en.ts";
import { LOCALES } from "./locales.ts";

export type { Key };

/** Коды, которые отдаёт moment.locale() или пишут по-другому, → код Obsidian. */
const ALIASES: Record<string, string> = {
  "zh-cn": "zh",
  "zh-hans": "zh",
  "zh-sg": "zh",
  "zh-hant": "zh-tw",
  "zh-hk": "zh-tw",
  "zh-mo": "zh-tw",
  nb: "no",
};

/** Языки с письмом справа налево: параметры в строках изолируются (пути, коды, адреса). */
const RTL = new Set(["ar", "dv", "fa", "he", "ur"]);

let lang = "en";
let locale = "en";
let dict: Dict = en;

function known(code: string): string | null {
  const c = ALIASES[code] ?? code;
  return c === "en" || Object.hasOwn(LOCALES, c) ? c : null;
}

/** Код словаря для языка Obsidian: точный код, затем базовый язык, иначе en. */
export function resolveLanguage(code: string): string {
  const c = code.trim().toLowerCase().replace(/_/g, "-");
  return known(c) ?? known(c.split("-")[0] ?? "") ?? "en";
}

export function setLanguage(code: string): void {
  lang = resolveLanguage(code);
  locale = code.trim().replace(/_/g, "-") || "en";
  dict = lang === "en" ? en : (LOCALES[lang] ?? en);
}

/** Текущий язык словаря (код из реестра или en). */
export function language(): string {
  return lang;
}

export function t(key: Key, params: Record<string, string | number> = {}): string {
  const s = dict[key] ?? en[key];
  const rtl = RTL.has(lang);
  return s.replace(/\{(\w+)\}/g, (m, k: string) => {
    if (!(k in params)) return m;
    // U+2068…U+2069: путь, код или адрес в арабской строке не переставляется по частям.
    return rtl ? `⁨${String(params[k])}⁩` : String(params[k]);
  });
}

export function formatBytes(n: number): string {
  const units = [t("unit.b"), t("unit.kb"), t("unit.mb"), t("unit.gb")];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatWith(ms: number, opts: Intl.DateTimeFormatOptions): string {
  try {
    return new Intl.DateTimeFormat(locale, opts).format(ms);
  } catch {
    return new Intl.DateTimeFormat(undefined, opts).format(ms);
  }
}

/** Дата и время в формате языка Obsidian. */
export function formatDateTime(ms: number): string {
  return formatWith(ms, { dateStyle: "short", timeStyle: "medium" });
}

/** Только время, в формате языка Obsidian. */
export function formatTime(ms: number): string {
  return formatWith(ms, { timeStyle: "medium" });
}

/** Только дата, в формате языка Obsidian. */
export function formatDate(ms: number): string {
  return formatWith(ms, { dateStyle: "short" });
}

/** Итог шага проверки соединения для людей. */
export function diagText(c: DiagCheck): string {
  const detail = c.detail;
  const byCode: Record<string, Key> = {
    tls: "diag.tls",
    dns: "diag.dns",
    refused: "diag.refused",
    timeout: "diag.timeout",
    network: "diag.network",
    not_selfsync: "diag.notSelfsync",
    headers_stripped: "diag.headersStripped",
    proto_major: "diag.protoMajor",
    server_old: "diag.serverOld",
    skew: "diag.skew",
    no_date: "diag.noDate",
    unauthorized: "diag.unauthorized",
    client_old: "diag.clientOld",
    rate_limited: "diag.rateLimited",
    server_limit: "diag.serverLimit",
    body_limit: "diag.bodyLimit",
    quota: "diag.quota",
    unexpected: "diag.unexpected",
    off: "diag.longpollOff",
    changed: "diag.changed",
    no_longpoll: "diag.noLongpoll",
    failed: "diag.longpollFailed",
  };
  const ok: Partial<Record<DiagCheck["step"], Key>> = {
    server: "diag.serverOk",
    version: "diag.versionOk",
    token: "diag.tokenOk",
    body: "diag.bodyOk",
    longpoll: "diag.longpollOk",
  };
  if (c.code === "ok") return t(ok[c.step] ?? "diag.fine", { detail });
  if (c.step === "token" && c.code === "read_only") return t("diag.tokenReadOnly");
  if (c.step === "body" && c.code === "read_only") return t("diag.bodyReadOnly");
  const key = byCode[c.code];
  return key ? t(key, { detail }) : `${c.code} ${detail}`.trim();
}

/** Строка о месте на сервере; `null` — сервер его не сообщает (старее 0.4.0). */
export function quotaText(q: QuotaStatus | undefined): string | null {
  if (!q || (q.used === 0 && q.soft === 0 && q.hard === 0 && q.held === 0)) return null;
  const parts = [q.hard > 0 ? t("quota.usedOf", { used: formatBytes(q.used), hard: formatBytes(q.hard) }) : t("quota.used", { used: formatBytes(q.used) })];
  if (q.soft > 0) parts.push(t("quota.soft", { soft: formatBytes(q.soft) }));
  const line = parts.join(", ");
  return q.held > 0 ? `${line}. ${t("quota.held", { n: q.held })}` : line;
}

/** Строка о хранилище на сервере; `null` — сервер о нём ничего не сообщил (старее 0.4.0). */
export function storageText(s: StorageHealth | undefined): string | null {
  if (!s || (s.verifiedAt <= 0 && s.damaged === 0 && !s.serverProblem)) return null;
  const parts: string[] = [];
  if (s.serverProblem) parts.push(t("storage.serverProblem"));
  if (s.damaged > 0) parts.push(t("storage.damaged", { n: s.damaged }));
  if (parts.length === 0) parts.push(t("storage.ok"));
  parts.push(s.verifiedAt > 0 ? t("storage.checked", { when: formatDateTime(s.verifiedAt) }) : t("storage.notChecked"));
  return parts.join("; ");
}

/** Текст уведомления; `null` — показывать не нужно. */
export function noticeText(n: Notice): string | null {
  switch (n.kind) {
    case "conflict":
      return t("notice.conflict", { path: n.path, copy: n.copy });
    case "localCopySaved":
      return t("notice.localCopySaved", { path: n.path, copy: n.copy });
    case "restoredEdited":
      return t("notice.restoredEdited", { path: n.path });
    case "restoredRemote":
      return t("notice.restoredRemote", { path: n.path });
    case "followedRename":
      return t("notice.followedRename", { from: n.from, to: n.to });
    case "rejected":
      return t("notice.rejected", { path: n.path, code: n.code });
    case "caseCollision":
      return t("notice.caseCollision", { path: n.path, existing: n.existing });
    case "skipped":
      return t("notice.skipped", { count: n.count });
    case "tooLarge":
      return t("notice.tooLarge", { path: n.path, size: formatBytes(n.size) });
    case "synced":
      // Пока только в журнал (см. showNotice).
      return null;
    case "recoveryPhrase":
      // Своё окно (showNotice), и никогда — в журнал.
      return null;
    case "readOnly":
      return t("notice.readOnly", { n: n.pending });
    case "quotaFull":
      return t("notice.quotaFull", { n: n.held });
    case "quotaSoft":
      return t("notice.quotaSoft", { used: formatBytes(n.used), soft: formatBytes(n.soft) });
    case "storageDamaged":
      return n.serverProblem ? t("notice.storageServerProblem") : t("notice.storageDamaged", { n: n.damaged });
    case "unsupportedName":
      return t("notice.unsupportedName", { path: n.path });
    case "nonPortableName":
      return t("notice.nonPortableName", { path: n.path });
    case "wrongPassword":
      return t("notice.wrongPassword");
    case "needPassword":
      return t("notice.needPassword");
    case "encryptionStarted":
      return t("notice.encryptionStarted");
    case "rekeyStarted":
      return t("notice.rekeyStarted");
    case "rekeyDone":
      return t("notice.rekeyDone");
    case "migrationProgress":
      return t("notice.migrationProgress", { done: n.done, total: n.total });
    case "encryptionEnabled":
      return t("notice.encryptionEnabled");
    case "passwordChanged":
      return t("notice.passwordChanged");
    case "unauthorized":
      return t("notice.unauthorized");
    case "protocolUnsupported":
      return t("notice.protocolUnsupported", { supported: n.supported });
    case "serverRewound":
      return t("notice.serverRewound");
    case "indexReset":
      return t("notice.indexReset");
    case "encryptionMismatch":
      return t("notice.encryptionMismatch");
    case "massDelete":
      return t("notice.massDelete", { count: n.count, total: n.total });
    case "settingsOverwritten": {
      // Полный список — в журнале (showNotice), здесь — начало.
      const paths = [...n.keptLocal, ...n.keptRemote].sort();
      const more = paths.length > 3 ? ` …+${paths.length - 3}` : "";
      return t("notice.settingsOverwritten", { paths: paths.slice(0, 3).join(", ") + more });
    }
    case "linksSkipped": {
      const more = n.paths.length > 3 ? ` …+${n.paths.length - 3}` : "";
      return t("notice.linksSkipped", { paths: n.paths.slice(0, 3).join(", ") + more });
    }
    case "error":
      return t("notice.error", { message: n.code === "read_only" ? t("readOnly.error") : n.message });
  }
}

export function stateText(s: SyncStatus | null): string {
  if (!s) return t("status.notConfigured");
  if (s.reason === "rate_limited" && s.retryAt > Date.now()) {
    return t("status.rateLimited", { time: formatTime(s.retryAt) });
  }
  if (s.readOnly && s.state === "idle") return t("status.readOnly");
  return t(`status.${s.state}`);
}
