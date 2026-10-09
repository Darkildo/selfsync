// Строки интерфейса ru/en. Язык — как у Obsidian (moment.locale()). Ядро присылает
// только коды и данные, весь текст формируется здесь.

import type { Notice, SyncStatus } from "./types.ts";

const en = {
  "status.idle": "Synced",
  "status.syncing": "Syncing",
  "status.offline": "Server unreachable",
  "status.error": "Sync error",
  "status.needPassword": "Password needed",
  "status.blocked": "Sync stopped",
  "status.notConfigured": "Not connected",
  "status.pending": "{n} pending",
  "status.conflicts": "{n} conflicts",
  "status.tooltip": "notesync: {state}. Click to sync now.",
  "status.lastSync": "last sync {time}",

  "cmd.syncNow": "Sync now",
  "cmd.connect": "Connect to server",
  "cmd.password": "Enter encryption password",
  "cmd.conflicts": "Show unresolved conflicts",
  "cmd.deleted": "Restore deleted files",
  "cmd.history": "File history",
  "cmd.log": "Show sync log",

  "notice.conflict": "Conflict in {path}: your version saved as {copy}",
  "notice.localCopySaved": "{path} differed from the server: your version saved as {copy}",
  "notice.restoredEdited": "{path} was deleted on another device but edited here — restored",
  "notice.restoredRemote": "{path}: deletion cancelled — the file was edited on another device",
  "notice.followedRename": "{from} was renamed to {to} on another device; your edits moved along",
  "notice.rejected": "Server refused {path} ({code})",
  "notice.caseCollision": "{path} differs from {existing} only by letter case — renamed on the server",
  "notice.tooLarge": "{path} is too large to sync ({size})",
  "notice.wrongPassword": "Wrong encryption password — sync stopped",
  "notice.needPassword": "This vault is encrypted: enter the password to sync",
  "notice.encryptionStarted": "Encryption is being enabled on another device — sync paused",
  "notice.migrationProgress": "Encrypting the vault: {done} / {total}",
  "notice.encryptionEnabled": "Encryption enabled",
  "notice.passwordChanged": "Encryption password changed",
  "notice.unauthorized": "The server rejected this device's token — connect again",
  "notice.protocolUnsupported": "The server speaks a different protocol version (v{supported}) — update the plugin or the server",
  "notice.serverRewound": "The server was restored from a backup — full reconciliation",
  "notice.indexReset": "Sync index was damaged — a safe full reconciliation started",
  "notice.encryptionMismatch": "The vault is encrypted here but not on the server — sync stopped",
  "notice.error": "Sync: {message}",

  "settings.connection": "Connection",
  "settings.connectedTo": "Connected to vault «{vault}» as «{device}»",
  "settings.notConnected": "Not connected. Use a link from the server or enter the address and token.",
  "settings.server": "Server address",
  "settings.serverDesc": "For example https://example.com/notesync",
  "settings.token": "Device token",
  "settings.tokenDesc": "Issued by `notesync token add` or received when connecting by code",
  "settings.connectByCode": "Connect by code",
  "settings.disconnect": "Disconnect",
  "settings.device": "Device name",
  "settings.deviceDesc": "Shown in conflict copies and the device list",
  "settings.sync": "Synchronisation",
  "settings.excludes": "Excluded paths",
  "settings.excludesDesc": "One per line: a file, a folder ending with /, * and ** wildcards",
  "settings.useWait": "Server is always running",
  "settings.useWaitDesc": "Socket or serve mode: wait for changes on the server instead of polling often. Turn off for CGI.",
  "settings.debounce": "Delay after an edit, seconds",
  "settings.pollActive": "Polling while active, seconds",
  "settings.pollIdle": "Maximum polling interval when idle, minutes",

  "connect.title": "Connect to notesync",
  "connect.server": "Server address",
  "connect.code": "One-time code",
  "connect.device": "Name of this device",
  "connect.submit": "Connect",
  "connect.done": "Connected to vault «{vault}»",
  "connect.failed": "Could not connect: {message}",

  "password.title": "Encryption password",
  "password.unlockDesc": "The vault is encrypted. Enter the password set when encryption was enabled.",
  "password.password": "Password",
  "password.remember": "Remember on this device",
  "password.rememberDesc": "The key is stored in this device's local storage, not in the vault",
  "password.submit": "Unlock",
  "password.strength": "Estimated strength: {bits} bits",
  "password.weak": "Weak password: anyone with the server's files can try to guess it",

  "common.cancel": "Cancel",

  "conflicts.title": "Unresolved conflicts",
  "conflicts.none": "No unresolved conflicts",
  "conflicts.desc": "The server version stays in place; yours was saved next to it.",
  "conflicts.copy": "Your version: {copy} · {time}",
  "conflicts.openServer": "Open the server version",
  "conflicts.openMine": "Open your version",
  "conflicts.keepBoth": "Keep both",
  "conflicts.keepMine": "Keep mine",
  "conflicts.keepServer": "Keep server's",
  "conflicts.resolved": "{path}: conflict resolved",

  "deleted.title": "Deleted files",
  "deleted.none": "Nothing was deleted recently",
  "deleted.restore": "Restore selected",
  "deleted.purge": "Delete forever",
  "deleted.meta": "{size} · deleted {when} · kept until {until}",
  "deleted.purgeConfirm": "Delete {n} files forever? This cannot be undone.",
  "deleted.restored": "Restored: {n}",

  "history.title": "History of {path}",
  "history.none": "The server has no history for this file",
  "history.rev": "Revision {rev} · {when}",
  "history.deleted": "deleted",
  "history.renamed": "renamed from {from}",
  "history.device": "device #{id}",
  "history.current": "current",
  "history.restore": "Restore",
  "history.restored": "Revision {rev} restored as a new version",
};

type Key = keyof typeof en;

const ru: Record<Key, string> = {
  "status.idle": "Синхронизировано",
  "status.syncing": "Синхронизация",
  "status.offline": "Сервер недоступен",
  "status.error": "Ошибка синхронизации",
  "status.needPassword": "Нужен пароль",
  "status.blocked": "Синхронизация остановлена",
  "status.notConfigured": "Не подключено",
  "status.pending": "к отправке: {n}",
  "status.conflicts": "конфликтов: {n}",
  "status.tooltip": "notesync: {state}. Нажмите, чтобы синхронизировать сейчас.",
  "status.lastSync": "последняя синхронизация {time}",

  "cmd.syncNow": "Синхронизировать сейчас",
  "cmd.connect": "Подключиться к серверу",
  "cmd.password": "Ввести пароль шифрования",
  "cmd.conflicts": "Нерешённые конфликты",
  "cmd.deleted": "Восстановить удалённые файлы",
  "cmd.history": "История файла",
  "cmd.log": "Журнал синхронизации",

  "notice.conflict": "Конфликт в {path}: ваша версия сохранена как {copy}",
  "notice.localCopySaved": "{path} отличался от серверного: ваша версия сохранена как {copy}",
  "notice.restoredEdited": "{path} удалили на другом устройстве, но здесь его правили — файл возвращён",
  "notice.restoredRemote": "{path}: удаление отменено — файл правили на другом устройстве",
  "notice.followedRename": "{from} переименован в {to} на другом устройстве; ваши правки перенесены",
  "notice.rejected": "Сервер не принял {path} ({code})",
  "notice.caseCollision": "{path} отличается от {existing} только регистром — переименован на сервере",
  "notice.tooLarge": "{path} слишком большой для синхронизации ({size})",
  "notice.wrongPassword": "Неверный пароль шифрования — синхронизация остановлена",
  "notice.needPassword": "Vault зашифрован: введите пароль, чтобы синхронизировать",
  "notice.encryptionStarted": "На другом устройстве включают шифрование — синхронизация на паузе",
  "notice.migrationProgress": "Шифрование vault'а: {done} / {total}",
  "notice.encryptionEnabled": "Шифрование включено",
  "notice.passwordChanged": "Пароль шифрования изменён",
  "notice.unauthorized": "Сервер не принял токен этого устройства — подключитесь заново",
  "notice.protocolUnsupported": "Сервер говорит на другой версии протокола (v{supported}) — обновите плагин или сервер",
  "notice.serverRewound": "Сервер восстановлен из резервной копии — полная сверка",
  "notice.indexReset": "Индекс синхронизации повреждён — начата безопасная полная сверка",
  "notice.encryptionMismatch": "Здесь vault зашифрован, а на сервере нет — синхронизация остановлена",
  "notice.error": "Синхронизация: {message}",

  "settings.connection": "Подключение",
  "settings.connectedTo": "Подключено к vault «{vault}» как «{device}»",
  "settings.notConnected": "Не подключено. Откройте ссылку с сервера или введите адрес и токен.",
  "settings.server": "Адрес сервера",
  "settings.serverDesc": "Например, https://example.com/notesync",
  "settings.token": "Токен устройства",
  "settings.tokenDesc": "Выдаёт `notesync token add`, либо он приходит при подключении по коду",
  "settings.connectByCode": "Подключить по коду",
  "settings.disconnect": "Отключить",
  "settings.device": "Имя устройства",
  "settings.deviceDesc": "Видно в конфликтных копиях и списке устройств",
  "settings.sync": "Синхронизация",
  "settings.excludes": "Исключения",
  "settings.excludesDesc": "По одному на строку: файл, папка с / на конце, шаблоны * и **",
  "settings.useWait": "Сервер работает постоянно",
  "settings.useWaitDesc": "Режим socket или serve: ждать изменений на сервере вместо частого опроса. Для CGI выключите.",
  "settings.debounce": "Пауза после правки, секунды",
  "settings.pollActive": "Опрос при активности, секунды",
  "settings.pollIdle": "Наибольший интервал опроса в простое, минуты",

  "connect.title": "Подключение к notesync",
  "connect.server": "Адрес сервера",
  "connect.code": "Одноразовый код",
  "connect.device": "Имя этого устройства",
  "connect.submit": "Подключить",
  "connect.done": "Подключено к vault «{vault}»",
  "connect.failed": "Не удалось подключиться: {message}",

  "password.title": "Пароль шифрования",
  "password.unlockDesc": "Vault зашифрован. Введите пароль, заданный при включении шифрования.",
  "password.password": "Пароль",
  "password.remember": "Запомнить на этом устройстве",
  "password.rememberDesc": "Ключ хранится в локальном хранилище устройства, не в vault'е",
  "password.submit": "Разблокировать",
  "password.strength": "Оценка стойкости: {bits} бит",
  "password.weak": "Слабый пароль: его можно подобрать, имея файлы сервера",

  "common.cancel": "Отмена",

  "conflicts.title": "Нерешённые конфликты",
  "conflicts.none": "Нерешённых конфликтов нет",
  "conflicts.desc": "Серверная версия осталась на месте, ваша сохранена рядом.",
  "conflicts.copy": "Ваша версия: {copy} · {time}",
  "conflicts.openServer": "Открыть серверную версию",
  "conflicts.openMine": "Открыть свою версию",
  "conflicts.keepBoth": "Оставить обе",
  "conflicts.keepMine": "Оставить свою",
  "conflicts.keepServer": "Оставить серверную",
  "conflicts.resolved": "{path}: конфликт решён",

  "deleted.title": "Удалённые файлы",
  "deleted.none": "Недавно ничего не удаляли",
  "deleted.restore": "Восстановить выбранные",
  "deleted.purge": "Удалить навсегда",
  "deleted.meta": "{size} · удалён {when} · хранится до {until}",
  "deleted.purgeConfirm": "Удалить навсегда файлов: {n}? Отменить будет нельзя.",
  "deleted.restored": "Восстановлено: {n}",

  "history.title": "История {path}",
  "history.none": "На сервере нет истории этого файла",
  "history.rev": "Ревизия {rev} · {when}",
  "history.deleted": "удалён",
  "history.renamed": "переименован из {from}",
  "history.device": "устройство №{id}",
  "history.current": "текущая",
  "history.restore": "Вернуть",
  "history.restored": "Ревизия {rev} возвращена новой версией",
};

let lang: "ru" | "en" = "en";

export function setLanguage(locale: string): void {
  lang = locale.toLowerCase().startsWith("ru") ? "ru" : "en";
}

export function t(key: Key, params: Record<string, string | number> = {}): string {
  const s = (lang === "ru" ? ru : en)[key];
  return s.replace(/\{(\w+)\}/g, (m, k: string) => (k in params ? String(params[k]) : m));
}

export function formatBytes(n: number): string {
  const units = lang === "ru" ? ["Б", "КБ", "МБ", "ГБ"] : ["B", "KB", "MB", "GB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
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
    case "tooLarge":
      return t("notice.tooLarge", { path: n.path, size: formatBytes(n.size) });
    case "wrongPassword":
      return t("notice.wrongPassword");
    case "needPassword":
      return t("notice.needPassword");
    case "encryptionStarted":
      return t("notice.encryptionStarted");
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
    case "error":
      return t("notice.error", { message: n.message });
  }
}

export function stateText(s: SyncStatus | null): string {
  if (!s) return t("status.notConfigured");
  return t(`status.${s.state}`);
}
