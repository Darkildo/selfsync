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
  "status.rateLimited": "Too many attempts, next one at {time}",
  "status.readOnly": "Read-only",
  "status.readOnlyPending": "{n} local changes will not be sent",
  "status.notConfigured": "Not connected",
  "status.pending": "{n} pending",
  "status.conflicts": "conflicts: {n}",
  "status.tooltip": "selfsync: {state}. Click to sync now.",
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
  "notice.unsupportedName": "{path}: this name is not allowed on this device — not synced here. Rename it on the device where it was created (for example, remove ? : * \" < > |)",
  "notice.nonPortableName": "{path}: this name is not allowed on Windows and Android — the file will not appear there. Rename it (for example, remove ? : * \" < > |)",
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
  "settings.serverDesc": "For example https://example.com/selfsync",
  "settings.token": "Device token",
  "settings.tokenDesc": "Issued by `selfsync token add` or received when connecting by code",
  "settings.connectByCode": "Connect by code",
  "settings.disconnect": "Disconnect",
  "settings.device": "Device name",
  "settings.deviceDesc": "Shown in conflict copies and the device list",
  "settings.sync": "Synchronisation",
  "settings.excludes": "Excluded paths",
  "settings.excludesDesc": "One per line: a file, a folder ending with /, * and ** wildcards; !pattern brings back what an earlier line excluded",
  "settings.useWait": "Server is always running",
  "settings.useWaitDesc": "Socket or serve mode: wait for changes on the server instead of polling often. Turn off for CGI.",
  "settings.debounce": "Delay after an edit, seconds",
  "settings.pollActive": "Polling while active, seconds",
  "settings.pollIdle": "Maximum polling interval when idle, minutes",

  "connect.title": "Connect to selfsync",
  "connect.server": "Server address",
  "connect.code": "One-time code",
  "connect.device": "Name of this device",
  "connect.submit": "Connect",
  "connect.done": "Connected to vault «{vault}»",
  "connect.failed": "Could not connect: {message}",
  "connect.rateLimited": "too many wrong codes from this address, wait {n} s",

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

  "encryption.heading": "Encryption",
  "encryption.on": "The vault is end-to-end encrypted: the server stores only ciphertext.",
  "encryption.off": "Off: the server stores notes as plain text.",
  "encryption.enable": "Enable encryption",
  "encryption.enableDesc": "All devices will need this password. A recovery phrase is shown next: it restores access if the password is forgotten. Update the plugin to 0.2.0 or newer on every device first — older versions cannot open such a vault.",
  "encryption.change": "Change password",
  "encryption.changeTitle": "Change encryption password",
  "encryption.oldPassword": "Current password",
  "encryption.newPassword": "New password",

  "retention.name": "Keep deleted files, days",
  "retention.desc": "How long the server keeps deleted files for restoring (for the whole vault)",

  "devices.heading": "Devices",
  "devices.current": "this device",
  "devices.revoked": "access revoked",
  "devices.lastSeen": "last seen {when}",
  "devices.revoke": "Revoke",
  "devices.revokeConfirm": "Revoke access for «{name}»? It will stop syncing immediately.",
  "devices.readOnly": "read-only",

  "join.title": "Connect a new device",
  "join.nameDesc": "A one-time link (valid for a short time): open it on the new device or scan the QR code",
  "join.namePlaceholder": "phone",
  "join.create": "Create link",
  "join.desc": "Scan with the new device's camera or open the link there. The link works once.",
  "join.code": "Code for manual entry: {code} · valid until {until}",
  "join.copy": "Copy link",
  "configSync.heading": "Obsidian settings",
  "configSync.appearance": "Appearance",
  "configSync.appearanceDesc": "Theme, fonts and installed themes (appearance.json, themes/)",
  "configSync.hotkeys": "Hotkeys",
  "configSync.hotkeysDesc": "hotkeys.json",
  "configSync.snippets": "CSS snippets",
  "configSync.snippetsDesc": "snippets/",
  "configSync.pluginList": "Plugin list",
  "configSync.pluginListDesc": "Which core and community plugins are on, and the plugin code itself — without their settings",
  "configSync.pluginSettings": "Plugin settings",
  "configSync.pluginSettingsDesc": "data.json of every plugin. Careful: settings are often device-specific (paths, keys). selfsync's own settings with the token never sync.",
  "configSync.editor": "Editor and general",
  "configSync.editorDesc": "app.json, graph.json, types.json. app.json also holds platform-specific options.",
  "configSync.rules": "Resulting rules",
  "configSync.rulesDesc": "Window layout (workspace*.json) and caches never sync. Your own exclusions above apply on top, including !pattern.",
  "configSync.rulesShow": "Show",
  "configSync.restart": "selfsync: Obsidian settings were updated from another device. Restart Obsidian for all of them to apply.",
  "recovery.title": "Recovery phrase",
  "recovery.showDesc": "These 24 words open the vault just like the password. They are shown only once: write them down on paper and keep them separate from the password. Anyone with the phrase can read the server copy.",
  "recovery.copy": "Copy",
  "recovery.copied": "Phrase copied — clear the clipboard after you saved it",
  "recovery.written": "I have written the phrase down",
  "recovery.done": "Done",
  "recovery.confirmFirst": "Confirm that the phrase is written down: it will not be shown again",
  "recovery.forgot": "Forgot the password?",
  "recovery.recoverTitle": "Restore access with the recovery phrase",
  "recovery.recoverDesc": "Enter the 24 words and a new password. Devices that remember the key keep working; the others will ask for the new password.",
  "recovery.phrase": "Recovery phrase",
  "recovery.recover": "Restore",
  "recovery.needPassword": "Enter a new password",
  "recovery.errCount": "The phrase has {n} words, 24 are needed",
  "recovery.errWord": "Word {n} is not in the BIP-39 word list — check the spelling",
  "recovery.errChecksum": "The words are valid but do not form this phrase: check the order and similar-looking words",
  "recovery.wrongPhrase": "This phrase does not open the vault (perhaps a newer one was created)",
  "recovery.restored": "Access restored, the password is replaced",
  "recovery.status": "Recovery phrase: {state}",
  "recovery.on": "set",
  "recovery.off": "not set",
  "recovery.create": "Create recovery phrase",
  "recovery.createNew": "Create a new phrase",
  "recovery.remove": "Remove phrase",
  "recovery.removed": "Recovery phrase removed",
  "recovery.wrongPassword": "Wrong encryption password",
  "recovery.createDesc": "Enter the encryption password. The previous phrase, if any, stops working. Devices need plugin 0.2.0 or newer: older versions cannot open a vault with a recovery phrase.",
  "recovery.removeDesc": "Enter the encryption password. Without a phrase, a forgotten password cannot be recovered.",
  "join.readOnly": "Read-only",
  "join.readOnlyDesc": "The new device receives changes but cannot send its own, change settings or connect other devices. Needs plugin 0.1.4 or newer on it.",
  "readOnly.desc": "This device is connected read-only: it receives changes from other devices, but its own edits stay here. Encryption, the retention period, revoking devices and new connect links are managed from a device with write access.",
  "readOnly.error": "This device is connected read-only",
  "notice.readOnly": "selfsync: this device is read-only — {n} local changes will not reach the server. If the same file changes elsewhere, your version is kept as a copy next to it.",
};

type Key = keyof typeof en;

const ru: Record<Key, string> = {
  "status.idle": "Синхронизировано",
  "status.syncing": "Синхронизация",
  "status.offline": "Сервер недоступен",
  "status.error": "Ошибка синхронизации",
  "status.needPassword": "Нужен пароль",
  "status.blocked": "Синхронизация остановлена",
  "status.rateLimited": "Слишком много попыток, следующая в {time}",
  "status.readOnly": "Только чтение",
  "status.readOnlyPending": "не уйдут на сервер: {n}",
  "status.notConfigured": "Не подключено",
  "status.pending": "к отправке: {n}",
  "status.conflicts": "конфликтов: {n}",
  "status.tooltip": "selfsync: {state}. Нажмите, чтобы синхронизировать сейчас.",
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
  "notice.unsupportedName": "{path}: такое имя недопустимо на этом устройстве — сюда не синхронизируется. Переименуйте его на устройстве, где он создан (например, уберите ? : * \" < > |)",
  "notice.nonPortableName": "{path}: такое имя недопустимо на Windows и Android — туда файл не попадёт. Переименуйте (например, уберите ? : * \" < > |)",
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
  "settings.serverDesc": "Например, https://example.com/selfsync",
  "settings.token": "Токен устройства",
  "settings.tokenDesc": "Выдаёт `selfsync token add`, либо он приходит при подключении по коду",
  "settings.connectByCode": "Подключить по коду",
  "settings.disconnect": "Отключить",
  "settings.device": "Имя устройства",
  "settings.deviceDesc": "Видно в конфликтных копиях и списке устройств",
  "settings.sync": "Синхронизация",
  "settings.excludes": "Исключения",
  "settings.excludesDesc": "По одному на строку: файл, папка с / на конце, шаблоны * и **; !шаблон возвращает исключённое строкой выше",
  "settings.useWait": "Сервер работает постоянно",
  "settings.useWaitDesc": "Режим socket или serve: ждать изменений на сервере вместо частого опроса. Для CGI выключите.",
  "settings.debounce": "Пауза после правки, секунды",
  "settings.pollActive": "Опрос при активности, секунды",
  "settings.pollIdle": "Наибольший интервал опроса в простое, минуты",

  "connect.title": "Подключение к selfsync",
  "connect.server": "Адрес сервера",
  "connect.code": "Одноразовый код",
  "connect.device": "Имя этого устройства",
  "connect.submit": "Подключить",
  "connect.done": "Подключено к vault «{vault}»",
  "connect.failed": "Не удалось подключиться: {message}",
  "connect.rateLimited": "слишком много неверных кодов с этого адреса, подождите {n} с",

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

  "encryption.heading": "Шифрование",
  "encryption.on": "Vault зашифрован сквозным шифрованием: сервер хранит только шифртекст.",
  "encryption.off": "Выключено: сервер хранит заметки открытым текстом.",
  "encryption.enable": "Включить шифрование",
  "encryption.enableDesc": "Этот пароль понадобится на всех устройствах. Следом покажется фраза восстановления: она вернёт доступ, если пароль забудется. Сначала обновите плагин до 0.2.0 или новее на всех устройствах — старые версии такой vault не откроют.",
  "encryption.change": "Сменить пароль",
  "encryption.changeTitle": "Смена пароля шифрования",
  "encryption.oldPassword": "Текущий пароль",
  "encryption.newPassword": "Новый пароль",

  "retention.name": "Хранить удалённое, дней",
  "retention.desc": "Сколько сервер хранит удалённые файлы для восстановления (для всего vault'а)",

  "devices.heading": "Устройства",
  "devices.current": "это устройство",
  "devices.revoked": "доступ отозван",
  "devices.lastSeen": "был в сети {when}",
  "devices.revoke": "Отозвать",
  "devices.revokeConfirm": "Отозвать доступ у «{name}»? Устройство сразу перестанет синхронизироваться.",
  "devices.readOnly": "только чтение",

  "join.title": "Подключить новое устройство",
  "join.nameDesc": "Одноразовая ссылка (действует недолго): откройте её на новом устройстве или отсканируйте QR-код",
  "join.namePlaceholder": "телефон",
  "join.create": "Создать ссылку",
  "join.desc": "Отсканируйте камерой нового устройства или откройте ссылку на нём. Ссылка срабатывает один раз.",
  "join.code": "Код для ручного ввода: {code} · действует до {until}",
  "join.copy": "Скопировать ссылку",
  "configSync.heading": "Настройки Obsidian",
  "configSync.appearance": "Внешний вид",
  "configSync.appearanceDesc": "Тема, шрифты и установленные темы (appearance.json, themes/)",
  "configSync.hotkeys": "Горячие клавиши",
  "configSync.hotkeysDesc": "hotkeys.json",
  "configSync.snippets": "Сниппеты CSS",
  "configSync.snippetsDesc": "snippets/",
  "configSync.pluginList": "Список плагинов",
  "configSync.pluginListDesc": "Какие встроенные и сторонние плагины включены и сам код плагинов — без их настроек",
  "configSync.pluginSettings": "Настройки плагинов",
  "configSync.pluginSettingsDesc": "data.json всех плагинов. Осторожно: настройки часто зависят от устройства (пути, ключи). Настройки самого selfsync с токеном не синхронизируются никогда.",
  "configSync.editor": "Редактор и общие",
  "configSync.editorDesc": "app.json, graph.json, types.json. В app.json есть и платформенные параметры.",
  "configSync.rules": "Итоговые правила",
  "configSync.rulesDesc": "Раскладка окон (workspace*.json) и кэши не синхронизируются никогда. Ваши исключения выше действуют поверх, в том числе !шаблон.",
  "configSync.rulesShow": "Показать",
  "configSync.restart": "selfsync: настройки Obsidian обновлены с другого устройства. Перезапустите Obsidian, чтобы применились все.",
  "recovery.title": "Фраза восстановления",
  "recovery.showDesc": "Эти 24 слова открывают vault так же, как пароль. Они показываются один раз: запишите их на бумагу и храните отдельно от пароля. Кто знает фразу, прочитает копию на сервере.",
  "recovery.copy": "Скопировать",
  "recovery.copied": "Фраза скопирована — очистите буфер обмена, когда сохраните её",
  "recovery.written": "Я записал фразу",
  "recovery.done": "Готово",
  "recovery.confirmFirst": "Отметьте, что фраза записана: больше она не покажется",
  "recovery.forgot": "Забыли пароль?",
  "recovery.recoverTitle": "Восстановление доступа фразой",
  "recovery.recoverDesc": "Введите 24 слова и новый пароль. Устройства, запомнившие ключ, продолжат работать, остальные попросят новый пароль.",
  "recovery.phrase": "Фраза восстановления",
  "recovery.recover": "Восстановить",
  "recovery.needPassword": "Введите новый пароль",
  "recovery.errCount": "Во фразе {n} слов, нужно 24",
  "recovery.errWord": "Слова {n} нет в словаре BIP-39 — проверьте написание",
  "recovery.errChecksum": "Слова правильные, но фраза не сходится: проверьте порядок и похожие слова",
  "recovery.wrongPhrase": "Эта фраза не открывает vault (возможно, создана более новая)",
  "recovery.restored": "Доступ восстановлен, пароль заменён",
  "recovery.status": "Фраза восстановления: {state}",
  "recovery.on": "есть",
  "recovery.off": "нет",
  "recovery.create": "Создать фразу восстановления",
  "recovery.createNew": "Создать новую",
  "recovery.remove": "Удалить фразу",
  "recovery.removed": "Фраза восстановления удалена",
  "recovery.wrongPassword": "Неверный пароль шифрования",
  "recovery.createDesc": "Введите пароль шифрования. Прежняя фраза, если была, перестанет работать. На устройствах нужен плагин 0.2.0 или новее: старые версии не откроют vault с фразой восстановления.",
  "recovery.removeDesc": "Введите пароль шифрования. Без фразы забытый пароль не восстановить.",
  "join.readOnly": "Только чтение",
  "join.readOnlyDesc": "Новое устройство будет получать изменения, но не сможет отправлять свои, менять настройки vault'а и подключать другие устройства. На нём нужен плагин 0.1.4 или новее.",
  "readOnly.desc": "Это устройство подключено только для чтения: оно получает изменения с других устройств, а его собственные правки остаются здесь. Шифрование, срок хранения удалённого, отзыв устройств и новые ссылки подключения — с устройства с правом записи.",
  "readOnly.error": "Устройство подключено только для чтения",
  "notice.readOnly": "selfsync: устройство только для чтения — локальных правок, которые не уйдут на сервер: {n}. Если тот же файл изменится на другом устройстве, ваша версия сохранится копией рядом.",
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
    case "synced":
      // Пока только в журнал (см. showNotice).
      return null;
    case "recoveryPhrase":
      // Своё окно (showNotice), и никогда — в журнал.
      return null;
    case "readOnly":
      return t("notice.readOnly", { n: n.pending });
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
      return t("notice.error", { message: n.code === "read_only" ? t("readOnly.error") : n.message });
  }
}

export function stateText(s: SyncStatus | null): string {
  if (!s) return t("status.notConfigured");
  if (s.reason === "rate_limited" && s.retryAt > Date.now()) {
    return t("status.rateLimited", { time: new Date(s.retryAt).toLocaleTimeString() });
  }
  if (s.readOnly && s.state === "idle") return t("status.readOnly");
  return t(`status.${s.state}`);
}
