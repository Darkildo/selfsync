# Протокол selfsync v1

Справочник для тех, кто пишет клиент или разбирается с сервером. Источник истины для форматов сообщений — [`proto/selfsync/v1/sync.proto`](../proto/selfsync/v1/sync.proto); здесь — порядок вызовов, коды ответов и смысл полей, который из схемы не виден.

## Общее

- Транспорт — HTTP/1.1, тела в protobuf (`content-type: application/x-protobuf`), содержимое блобов — `application/octet-stream`.
- Версия протокола — заголовок `x-selfsync-proto: 1` в каждом запросе. На несовместимую версию сервер отвечает `426` с `Error.supported_proto`.
- Авторизация — `Authorization: Bearer <токен устройства>`. Токен определяет и устройство, и vault, поэтому имени vault'а в путях нет.
- Ошибка — HTTP-статус и тело `Error { code, message }`. `code` машинный (`unauthorized`, `bad_request`, `not_found`, `conflict`, `too_large`, …), `message` — для людей, без внутренних деталей.
- Время — миллисекунды Unix. `mtime` в записях задаёт клиент, это информация, а не основание для решений. Порядок событий задаёт только `seq`.

## Модель данных

У каждого vault'а есть лог изменений. Каждая запись (`Entry`) — текущее состояние одного пути:
- `seq` — номер записи в логе. Глобальный и монотонный, при каждом изменении пути запись получает новый seq.
- `rev` — ревизия пути (1, 2, 3…). Основа оптимистичной блокировки: операция говорит, от какой ревизии сделана правка (`base_rev`).
- `hash` — SHA-256 хранимого блоба (у tombstone и папок пусто).
- `deleted` — tombstone. Удалённое хранится `retention_days` (по умолчанию 30), потом его стирает sweep. Об этом говорит `VaultState.purged_seq`.
- `renamed_from` / `renamed_to` — связь записей переименования: у новой записи и у tombstone старого пути.

Путь — `Path { segments, encrypted }`. Открытый путь — сегменты в UTF-8 NFC; сервер проверяет каждый сегмент по правилам `selfsync_core::path` (без `.`/`..`, управляющих символов и запрещённых в Windows имён). Зашифрованный путь — сегменты-шифртексты, сервер видит только их длину.

## Получение изменений

`GET /v1/changes?since=<seq>&limit=<n>` → `ChangesResponse { entries, next_seq, has_more, vault }`

Записи с `seq > since` по возрастанию seq. Клиент хранит курсор и сдвигает его на `next_seq` только после того, как применил всю пачку. В `vault` (`VaultState`) приходят текущая голова лога, режим шифрования, флаг миграции, окно хранения и `purged_seq`. Если `purged_seq > since`, клиент мог пропустить окончательно стёртые записи и должен сделать полную сверку (`since=0`).

`GET /v1/wait?since=<seq>&timeout=<с>` → `VaultState`

Long-poll: сервер отвечает, как только голова лога ушла дальше `since`, или по таймауту (до 25 с). В режиме CGI отвечает сразу: клиент это замечает и переходит на обычный опрос.

## Отправка изменений

### Блобы

1. `POST /v1/blobs/missing` (`HashList`) → `HashList` — каких блобов нет на сервере.
2. Маленький блоб (до 8 МиБ): `PUT /v1/blobs/{hash}` с телом блоба. Сервер проверяет хэш.
3. Большой — частями с докачкой:
   - `POST /v1/uploads` (`UploadStart { hash, size }`) → `UploadState { upload_id, offset, complete }`. Если `complete`, блоб уже есть;
   - `PUT /v1/uploads/{id}` с `Content-Range: bytes <начало>-<конец>/<всего>` — очередная часть (по 4 МиБ). Если сервер ждёт другое смещение, он отказывает, и клиент спрашивает состояние;
   - `GET /v1/uploads/{id}` → `UploadState` — сколько принято (после обрыва);
   - `POST /v1/uploads/{id}/commit` — проверка хэша и перенос в хранилище.

`GET /v1/blobs/{hash}` отдаёт блоб, поддерживает `Range` и `ETag`. Большие блобы клиент качает диапазонами по границам чанков.

### Операции

`POST /v1/ops` (`OpsRequest { ops }`) → `OpsResponse { results, vault }`, по одному результату на операцию в том же порядке. Операции применяются по одной, каждая в своей транзакции:

| Операция | Смысл |
|---|---|
| `Put { path, base_rev, hash, size, mtime }` | записать содержимое; `base_rev = 0` — новый файл (допустимо поверх tombstone) |
| `Delete { path, base_rev }` | удалить (tombstone) |
| `Rename { from, to, base_rev }` | переименовать файл или папку (`base_rev` — ревизия источника) |
| `Mkdir { path }` | создать пустую папку |

Результат:
- `Applied { rev, seq, noop }`. При `noop` состояние уже совпадало, и seq не сдвинулся. Повтор после потерянного ответа получает именно его, а не конфликт: идемпотентность проверяется раньше `base_rev`.
- `Conflict { server, at_destination }` — `base_rev` устарел, в `server` текущее состояние пути. Для `Rename` `at_destination` значит, что занято место назначения, и тогда `server` описывает запись назначения.
- `MissingBlob` — блоба нет (его стёр gc между загрузкой и операцией), нужно залить заново.
- `Rejected { code, message }` — операцию нельзя выполнить никогда в таком виде: `invalid_path`, `too_large`, `plaintext_in_encrypted_vault`, …

Переименование: запись назначения получает seq меньше, чем tombstone источника. Клиент, увидевший tombstone, уже видел новый путь.

## История и корзина

- `GET /v1/history?path=<base64url(Path)>` → `HistoryResponse` — ревизии пути от новых к старым. Вернуть старую ревизию — это `Put` её хэша поверх текущей.
- `GET /v1/deleted` → `DeletedResponse` — tombstone'ы в окне хранения, у каждого `last_live` (что восстанавливать) и `expires_at`.
- `DELETE /v1/deleted` (`PathList`) → `PurgeResult` — стереть окончательно: историю и блобы, на которые больше никто не ссылается.
- `GET|PUT /v1/retention` (`Retention { days }`) — окно хранения удалённого.
- `GET /v1/stats` → `Stats`.

## Шифрование

- `GET /v1/vaultkey` → `VaultKeyResponse { record, version }`. Пустая запись означает, что vault не зашифрован. `version` повторяется в `ETag`.
- `PUT /v1/vaultkey` (`VaultKeyPut`) с `If-Match: <version>` (0 — записи ещё нет) → `VaultKeyResponse`. При несовпадении версии — `412`, запись сменило другое устройство. `VaultKeyRecord` — обёртка мастер-ключа: параметры Argon2id, соль, nonce, `wrapped_key`, `key_check`.

### Миграция существующего vault'а

1. `PUT /v1/vaultkey` — запись ключа. С этого момента открытые записи отклоняются, кроме времени, пока стоит маркер.
2. `PUT /v1/vaultkey/migration` (`MigrationMarker { key_version }`) — маркер. Остальные устройства видят `VaultState.migration` и ставят запись на паузу. Повтор от того же устройства — no-op; чужой маркер — `409 migration_in_progress`.
3. Клиент перезаливает каждую живую открытую запись зашифрованной.
4. `POST /v1/vaultkey/migration/purge` (`MigrationPurge { max_seq }`) — стереть все открытые записи вместе с историей и в той же транзакции снять маркер. Если есть живая открытая запись новее `max_seq`, сервер отвечает `409 plaintext_changed`: её нужно доперезалить. `409 no_migration` на повтор означает, что purge уже прошёл.
5. `DELETE /v1/vaultkey/migration` — снять маркер (после purge — no-op).

Если устройство бросило миграцию на полпути (ключ есть, маркера нет, открытые записи живы), её доводит любое устройство с ключом. Правила — в [decisions.md](decisions.md#миграция-на-шифрование-раздел-94).

## Устройства и подключение

- `GET /v1/devices` → `DevicesResponse { devices, self_id, vault }`.
- `DELETE /v1/devices/{id}` — отозвать: токен перестаёт приниматься (`401`).
- `POST /v1/join` (`JoinCreate { name }`) → `JoinCode { code, url, expires_at }` — одноразовый код на 15 минут.
- `POST /v1/join/redeem` (`JoinRedeem { code, name }`, без авторизации) → `JoinToken` — токен нового устройства (показывается один раз).
- `GET /join/{code}` — HTML-страница для человека со ссылкой `obsidian://selfsync-connect?server=…&code=…`.
- `GET /v1/health` (без авторизации) — `200`, если сервер жив и база открывается.
