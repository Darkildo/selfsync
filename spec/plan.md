# План реализации OneWaySync / notesync

Документ живой: отмечаем этапы по мере готовности, отклонения — в `decisions.md`.

## Крейты и модули

```
crates/
  notesync-proto     build.rs: prost-build + protox (без системного protoc)
                     lib.rs: pub mod v1 (сгенерированное), PROTO_VERSION = 1, HEADER_PROTO
  notesync-core      sans-IO, no tokio, компилируется в wasm32
    path.rs          VaultPath (сегменты), NFC-валидация/нормализация, каноническое
                     кодирование для БД, имена конфликтных копий, casefold
    hash.rs          Hash([u8;32]), sha256-хелперы, hex
    blob.rs          формат NSB: заголовок, plain/encrypted, чанки 1 МиБ, lz4 для текста,
                     потоковые кодер/декодер, расшифровка отдельного чанка
    crypto.rs        VaultKeyRecord, Argon2id KEK, wrap/unwrap, HKDF-подключи,
                     AES-256-GCM-SIV для чанков, AES-SIV + паддинг для сегментов имён
    merge.rs         построчный diff3 (поверх similar::Myers), \r\n, без \n в конце
    exclude.rs       шаблоны исключений (точный путь, префикс каталога, * и **)
    index.rs         LocalIndex: FileState на путь, last_seq, device, миграция,
                     незавершённые передачи; postcard-снимок с версией формата
    proto_io.rs      построители HTTP-запросов и разбор ответов (protobuf)
    engine/          Engine::handle(Event) -> Vec<Action>
      mod.rs         внешний интерфейс, Event/Action, планирование таймеров
      runtime.rs     мини-исполнитель корутин (noop waker): IO как ожидание события
      sync.rs        цикл синка: скан → pull → push → разрешение конфликтов
      transfer.rs    загрузки/скачивания (resumable, Range)
      conflict.rs    правила 8.4
      crypto_flow.rs ключ, пароль, смена пароля, миграция 9.4
      schedule.rs    debounce, адаптивный опрос, приоритеты
  notesync-wasm      wasm-bindgen: WasmEngine { handle(event) -> actions }, утилиты
  notesync-server    lib + bin `notesync`
    config.rs        env/TOML/CLI
    db/{server,vault,migrate}.rs  SQLite, PRAGMA, миграции user_version
    blobs.rs         хранилище по хэшу, uploads, Range
    api/{mod,auth,ops,changes,blobs,uploads,history,trash,vaultkey,devices,join,error}.rs
    modes/{cgi,socket,serve,idle}.rs
    cmd/{token,link,import,gc,sweep,backup,healthcheck,migrate,vault}.rs
    sweep.rs, joinpage.rs (HTML), qr.rs (терминал)
  notesync-sim       N клиентов (Engine) + сервер in-process (Router::oneshot) +
                     фейковые ФС/сеть/часы, seed-управляемые сбои, инварианты
  notesync-cli       (этап 9) нативный исполнитель для папки, notify
plugin/              TS: main, io (исполнитель), transport, scheduler, ui/*, i18n; esbuild
                     встраивает wasm в main.js; test/ — заглушка Obsidian + настоящий сервер
```

## Этапы (раздел 14)

1. [x] Фундамент: workspace, proto, path, blob(plain), сервер (схема, changes, ops, blobs,
   токены, serve), каркас симуляции + сценарии 1–3.
2. [x] Режимы запуска: cgi, socket, idle-выход, sweeper, деплой, Docker; тесты 13.4.
   Docker-образ не собирался (на машине нет Docker).
3. [x] Ядро синка: индекс, планировщик, diff3, правила 8.3–8.4, rename, корзина; все сценарии sim.
   40 000 случайных прогонов без нарушений; сценарии 13.2 — `notesync-sim/tests/scenarios.rs`.
   Большие файлы и шифрование в ядре уже есть (этапы 6–7 остаются за плагином).
4. [x] Плагин MVP: wasm-сборка, исполнитель, ручное подключение, ручной синк.
5. [x] Полный синк в плагине: debounce, опрос, wait, окна конфликтов/восстановления, статус-бар.
6. [x] Большие файлы: resumable в обе стороны, приоритеты, чтение частями на десктопе.
7. [x] Шифрование: ключи, чанки, имена, смена пароля, миграция.
8. [x] Продукт: QR, устройства, history UI, import, gc, ru/en, релизный CI.
   Не проверено на живом Obsidian и телефонах (e2e — на заглушке API); CI не прогонялся.
9. [x] CLI-клиент.
