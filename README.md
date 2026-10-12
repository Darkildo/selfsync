# selfsync

**English** · [Русский](README.ru.md)

Sync your Obsidian vault through a server you run yourself. One small Rust binary, data in SQLite and plain files, a plugin for Obsidian on desktop and mobile, and a command-line client for servers and NAS boxes.

The main promise: **no edit is ever lost silently.**
- Edits to different parts of a file are merged automatically (diff3).
- When edits overlap, both versions are kept: the server's in place, yours as a copy next to it.
- An edit wins over a delete.
- Files deleted on any device stay in the server's trash, and every file has a revision history.

Also:
- renames and moves without re-uploading;
- resumable transfers of large files in both directions;
- case-insensitive file systems (Windows, macOS, iOS, Android) and Unicode names (NFC/NFD);
- optional end-to-end encryption of contents and file names;
- connecting a new device with a link or a QR code;
- revoking devices.

## Before you install the plugin

- **You need your own selfsync server.** The plugin does nothing without one; there is no hosted service, account or subscription. Setup is described below.
- **Network use.** The plugin talks only to the server address you enter: it uploads and downloads the files of your vault and their metadata (names, sizes, modification times). Without encryption the server stores your notes as they are; with encryption it stores only ciphertext.
- **No telemetry.** The plugin sends nothing anywhere else.
- **Direct file system access on desktop.** Obsidian's review flags this, so here is why. On desktop the plugin reads and writes the files of your vault through Node's `fs` module instead of Obsidian's API: a downloaded file replaces the old one atomically and is flushed to disk (`fsync`), so a crash or power loss never leaves a half-written note, and large attachments are read and hashed in chunks instead of being loaded into memory whole. It only touches paths inside the vault folder (plus Obsidian's trash when a file is deleted). On mobile it uses Obsidian's adapter API.
- **Clipboard.** The "Copy link" button in "Connect a new device" writes the one-time connect link to the clipboard. The plugin never reads the clipboard.
- **WebAssembly inside `main.js`.** The sync core is Rust compiled to WebAssembly ([crates/selfsync-wasm](crates/selfsync-wasm)) and embedded into `main.js`, which is minified. Nothing is obfuscated or downloaded at runtime: release assets are built from this repository by the [release workflow](.github/workflows/release.yml) and carry GitHub build provenance attestations.
- Deleted files go to Obsidian's trash (system or `.trash/`, as configured in Obsidian), never deleted permanently.

## How it works

```
 Obsidian (desktop, mobile)         folder on a server/NAS
 ┌──────────────────────────┐       ┌──────────────────┐
 │ plugin (TypeScript)      │       │ selfsync-cli     │
 │  I/O and UI              │       │  tokio + std::fs │
 │ ┌──────────────────────┐ │       │ ┌──────────────┐ │
 │ │ sync core (WASM)     │ │       │ │ sync core    │ │
 │ └──────────────────────┘ │       │ └──────────────┘ │
 └────────────┬─────────────┘       └────────┬─────────┘
              │      HTTP + protobuf         │
              └──────────────┬───────────────┘
                     ┌───────┴────────┐
                     │ selfsync       │  SQLite (metadata, history)
                     │ (server)       │  + content-addressed blobs
                     └────────────────┘
```

The sync logic is written once, in the `selfsync-core` crate. The plugin and the command-line client only carry out its actions: read a file, write a file, send a request. Design documents (in Russian): [architecture](spec/architecture.md), [protocol](spec/protocol.md), [decisions](spec/decisions.md).

## Server

### Choosing a mode

The server is a single `selfsync` binary that can run in several ways. They differ in how much they cost while nobody is syncing and how fast an edit on one device shows up on another.

| Mode | Process between syncs | How clients learn about changes | Good for |
|---|---|---|---|
| **CGI** (Caddy, nginx + fcgiwrap) | none, one process per request (~1 ms start) | polling: every 15 s while Obsidian is active, backing off to 5 min when idle | shared hosting, minimal resources |
| **socket** (systemd socket activation) | started by the first request, exits after 10 min idle | long-poll `/v1/wait`, nearly instant | your own VPS: fast and almost free when idle |
| **serve** (Docker, plain process) | always running | long-poll | Docker, home server |
| **serve + Sablier** (Docker on demand) | the proxy starts the container, Sablier stops it | polling | Docker with memory savings |
| **Podman Quadlet** | like socket, in a container | long-poll | rootless containers |

**Long-poll versus starting on demand is a trade-off.** Long-poll keeps a request open while Obsidian is in the foreground, which in socket mode keeps the process alive until 10 minutes after every device goes to the background. If you would rather have no process almost all the time, turn off "Server is always running" in the plugin: clients will poll instead, and changes from other devices will arrive up to one polling interval later. Keep it off for CGI, where long-poll is impossible (the client detects this and switches to polling on its own).

### Installing the binary

Static binaries (musl, x86_64 and aarch64) are published in `server-X.Y.Z` releases together with `SHA256SUMS` and build provenance attestations (the `X.Y.Z` releases hold only the Obsidian plugin):

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify selfsync-x86_64-unknown-linux-musl --repo Darkildo/selfsync
sudo install -m 0755 selfsync-x86_64-unknown-linux-musl /usr/local/bin/selfsync
```

From source: `cargo build --release -p selfsync-server` (binary at `target/release/selfsync`).

### systemd: start on demand (socket)

```sh
sudo cp deploy/systemd/selfsync.{socket,service} deploy/systemd/selfsync-{sweep,verify,backup}.{service,timer} /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now selfsync.socket selfsync-sweep.timer selfsync-verify.timer selfsync-backup.timer
```

Put a reverse proxy with TLS in front of the server. Ready-made configs: [deploy/caddy/Caddyfile.socket](deploy/caddy/Caddyfile.socket) and [deploy/nginx/selfsync-proxy.conf](deploy/nginx/selfsync-proxy.conf). The unit runs with `DynamicUser`, data lives in `/var/lib/selfsync`. Run admin commands like this:

```sh
sudo systemd-run --pipe -p DynamicUser=yes -p User=selfsync -p StateDirectory=selfsync \
  /usr/local/bin/selfsync token add --vault notes --name laptop
```

### CGI

[deploy/caddy/Caddyfile.cgi](deploy/caddy/Caddyfile.cgi) (Caddy with the `caddy-cgi` plugin) or [deploy/nginx/selfsync-cgi.conf](deploy/nginx/selfsync-cgi.conf) (nginx + fcgiwrap). CGI mode is detected from `GATEWAY_INTERFACE`. The data directory must belong to the user CGI runs as, and admin commands must run as that user too.

### Docker

```sh
cd deploy
mkdir -p data && sudo chown 65532:65532 data   # bind mount, not a named volume
docker compose --profile always up -d           # or --profile ondemand (Sablier)
docker compose run --rm selfsync token add --vault notes --name laptop
```

The image is based on distroless/static: no shell, read-only file system, unprivileged process. Data is kept in `./data` through a bind mount rather than a named volume, because `docker compose down -v` deletes named volumes together with all your notes.

### Podman Quadlet

[deploy/quadlet/](deploy/quadlet/) runs the container with socket activation under rootless Podman.

### Configuration

Environment variables (or TOML via `--config`; variables take precedence):

| Variable | Default | Meaning |
|---|---|---|
| `SELFSYNC_DATA_DIR` | `$STATE_DIRECTORY` or `/var/lib/selfsync` | data directory |
| `SELFSYNC_PUBLIC_URL` | from request headers | external address, used in connect links |
| `SELFSYNC_IDLE_TIMEOUT` | `10m` (socket) | exit after idling, `0` means never |
| `SELFSYNC_MAX_BLOB_SIZE` | 512 MiB | largest file |
| `SELFSYNC_MAX_BODY_SIZE` | 16 MiB | request body (metadata, upload chunk); keep in line with the proxy |
| `SELFSYNC_LOG` | `info` | log level (file names never reach the log) |
| `SELFSYNC_TRUSTED_PROXY` | `0` | `1`: take the client address from `X-Forwarded-For` (rightmost entry) |
| `SELFSYNC_METRICS_TOKEN` | — | enables `GET /metrics` behind this bearer token (16 characters or more) |
| `SELFSYNC_RATE_REDEEM_ATTEMPTS` | `10` | wrong connect codes from one address per window |
| `SELFSYNC_RATE_REDEEM_WINDOW` | `15m` | counting window for connect codes |
| `SELFSYNC_RATE_REDEEM_BLOCK` | `15m` | how long an address is blocked after the limit |
| `SELFSYNC_RATE_REDEEM_GLOBAL` | `200` | wrong connect codes from all addresses in the same window; past it, one attempt a minute |
| `SELFSYNC_RATE_AUTH_ATTEMPTS` | `30` | rejected tokens (401) from one address per window |
| `SELFSYNC_RATE_AUTH_WINDOW` | `1m` | counting window for tokens |
| `SELFSYNC_RATE_AUTH_BLOCK` | `1m` | how long an address is blocked after the limit |

In TOML the limits go into a `[rate]` section with the same names in lower case and without the prefix (`redeem_attempts = 10`, `auth_window = "1m"`); `trusted_proxy = true` is a top-level key. A threshold of `0` turns that limit off; windows and blocks must be at least one second, otherwise the server refuses to start.

**Failed-attempt limits.** Codes and tokens are far too long to guess, but a scanner should neither load SQLite nor look like normal traffic in the log. The server counts only failures (a wrong connect code, a rejected token) and answers `429` with `Retry-After` once a limit is reached; the plugin and the command-line client wait that long without retrying. Counters live in `server.db`, so they work in CGI mode too; `selfsync sweep` removes those idle for a day. Requests without a token are not counted. The client address is the connection address (in CGI, `REMOTE_ADDR`). Behind a reverse proxy in `serve`/`socket` mode every request comes from the proxy: set `SELFSYNC_TRUSTED_PROXY=1` and let the proxy pass the address (the configs in [deploy/](deploy/) already do). Never enable it without a proxy that overwrites the header — anyone could then pick their own address. Without a known address (Unix socket, no trusted proxy) all clients share one counter.

**Metrics.** With `SELFSYNC_METRICS_TOKEN` set, `GET /metrics` returns Prometheus text format; without it the route does not exist (404), with a wrong token it is 401. What it shows: server version and start time; per vault — files, folders, trash, history, current-version bytes, blob count and size, encryption, blobs requested from devices, and the last `selfsync verify` result (time, damaged and missing blobs, database errors, key record problem); per device — last contact time, client version, read-only flag; addresses currently blocked by the limiter; counters of 401, 403, 426, 429 and 5xx answers. Counters are kept in `server.db`, so a server that exits when idle does not lose them, and scraping does not count as activity: it does not keep a socket-activated server running. Device names are visible to whoever holds the token. Example:

```yaml
scrape_configs:
  - job_name: selfsync
    scheme: https
    scrape_interval: 1m
    authorization:
      credentials: <SELFSYNC_METRICS_TOKEN>
    static_configs:
      - targets: ["sync.example.com"]
```

Each scrape lists the blob directory, so an interval of a minute or more is enough. A device silent for a week: `time() - selfsync_device_last_seen_timestamp_seconds > 7*86400`.

### Admin commands

```sh
selfsync token add --vault notes --name laptop   # device token (the vault is created on first use)
selfsync link --vault notes --name phone         # one-time link + QR code in the terminal
selfsync token add --vault notes --name nas --read-only   # receives changes, cannot send any
selfsync token list | revoke --name phone      # list shows each device's client (plugin/0.3.0)
selfsync vault list
selfsync import --vault notes --from ~/Notes     # upload an existing folder
selfsync backup /backups/selfsync-$(date +%F)    # consistent copy while running
selfsync gc --vault notes                         # garbage collection plan; --yes to run it
selfsync sweep                                    # erase deleted files older than the retention window (timer)
selfsync verify                                   # integrity check: databases, blob hashes, missing blobs (timer)
selfsync healthcheck
```

Back up with `backup`: the copy is consistent even while the server is running. To restore, put the directory back. Devices notice that the server went back in time and run a full reconciliation without deleting anything. What a backup protects against depends on how blobs get into it:

| Command | Blobs | Protects against |
|---|---|---|
| `selfsync backup DIR` | hard links to the storage (copies on another file system) | `gc`, mistakes, a lost database; **not** a byte going bad on disk: a damaged blob is damaged in both places |
| `selfsync backup DIR --copy` | real copies | also corruption of the storage |
| `selfsync backup --rotate ROOT --keep 7 --copy` | real copies; between backups shared hard links, so each new one takes only the changes | the same, with the last 7 copies |

`--rotate` writes each copy to `ROOT/<UTC time>` (first under a hidden `.partial` name, renamed when done), then removes the oldest beyond `--keep`; it only touches directories with its own naming. The `selfsync-backup.timer` unit does this daily into `/var/lib/selfsync-backups`. Mount another disk there: a copy on the same disk does not survive a failed disk. A backup is checked like the storage itself: `selfsync --data /var/lib/selfsync-backups/<time> verify`.

**Integrity check.** A silently damaged blob would otherwise surface only when some device tries to download it, maybe a year later. `selfsync verify` checks both databases (`PRAGMA integrity_check`), recomputes the SHA-256 of every blob (the name of a blob is the hash of its stored bytes, encrypted ones included, so no key is needed), looks for blobs that records refer to but that are gone, and checks that an encrypted vault still has a readable key record. Blobs nobody refers to and abandoned uploads are only counted, `gc` removes them. Exit code: 0 — clean, 1 — problems found, 2 — could not run. `--quiet` prints only problems. The check is safe on a running server.

**Repair from devices.** A damaged or missing blob that `verify` could not take from a backup is requested from the devices: the damaged file is moved to `quarantine/`, and any device (plugin or `selfsync-cli` 0.3.2+, not read-only) whose copy of the file matches the server rebuilds the blob and uploads it on its next sync. The server checks the hash. Blobs that only old versions in the history refer to cannot be rebuilt this way; `verify` lists those still waiting after a day.

A damaged or missing blob can be taken from any backup, since storage is content-addressed and the backup holds the same file under the same name: `selfsync verify --repair-from /backups/selfsync-2026-10-01`. Every file from the backup is checked against its hash first; the damaged original is moved to `quarantine/` in the vault directory (`gc` leaves it alone, `backup` copies it).

The `selfsync-verify.timer` unit runs the check weekly; when problems are found the service fails, so it shows up in `systemctl --failed`, and you can hang your own `OnFailure=` notification on it. A one-off run under `DynamicUser`, and in Docker:

```sh
sudo systemd-run --pipe -p DynamicUser=yes -p User=selfsync -p StateDirectory=selfsync \
  /usr/local/bin/selfsync verify --repair-from /backups/selfsync-2026-10-01
docker compose run --rm selfsync verify
```

### Read-only devices

A mirror on a publishing server, a backup copy on a NAS, a reader on an old tablet: a device that should change nothing, and whose token gives no write access if it leaks.

```sh
selfsync token add --vault notes --name nas --read-only
selfsync link --vault notes --name tablet --read-only
```

In the plugin, turn on "Read-only" under "Connect a new device" before creating the link. The rule on the server is a single one: a read-only device may only send `GET` and `HEAD`. So it cannot write files, change the retention period or the encryption key, revoke devices, or create connect links. `token list` and the device list in the plugin mark such devices.

The device receives everything, and its own edits stay on it: the status bar shows "Read-only" and how many local changes will not be sent. If a file edited there also changes on another device, the server version takes its place and the local one is kept as a copy next to it, as in a conflict — nothing is lost silently. The plugin hides the buttons for encryption, retention, revoking and connect links. `selfsync-cli run --once` in such a folder exits with code 0 and prints a warning about local changes; `status` shows the mode. Plugin and CLI older than 0.2.1 do not know this mode: the server refuses them and they ask to be updated. A read-only connect link is not used up by such a client, so it still works after the update. The other way round, a plugin connected to a server older than 0.2.1 does not offer read-only links, because that server would silently give full access.

## Obsidian plugin

### Installing

From Obsidian's community plugins (search for "Selfsync"), or by hand: copy `main.js`, `manifest.json` and `styles.css` from a release into `<vault>/.obsidian/plugins/selfsync/` and enable the plugin under Settings → Community plugins.

### Connecting

- **With a link or QR code.** Run `selfsync link …` on the server, or press "Connect a new device" in the settings of an Obsidian that is already connected. Open the link on the new device (a phone camera reads the QR code): the server page opens Obsidian with the fields filled in.
- **With a code** — the "Connect to server" command.
- **With a token** — server address and token in the plugin settings.

### Day to day

- **Status bar:** ✓ synced, ↻ syncing, ⚠ server unreachable, 🔒 password needed, ⛔ sync stopped. Click to sync now (or enter the password, or reconnect).
- **Sync** runs 2–3 seconds after the last edit, on startup, when Obsidian returns to the foreground, and on a polling schedule. When Obsidian goes to the background, pending changes are sent right away.
- **Conflicts** raise a notice; the "Unresolved conflicts" window lets you keep both versions, yours or the server's.
- **Deleted files** — the "Restore deleted files" command opens the server's trash.
- **File history** — from the file context menu or a command.

The vault's configuration folder (`.obsidian/` by default; settings differ between devices) and `.trash/` are not synced by default. Exclusions are configurable: one pattern per line, and `!pattern` brings back something an earlier line excluded (the last matching line wins, as in `.gitignore`).

### Obsidian settings between devices

Under "Obsidian settings" in the plugin settings, choose what to carry across. Everything is off by default, which is how 0.1.x behaved.

| Toggle | Syncs | Note |
|---|---|---|
| Appearance | `appearance.json`, `themes/` | theme and fonts |
| Hotkeys | `hotkeys.json` | |
| CSS snippets | `snippets/` | |
| Plugin list | `community-plugins.json`, `core-plugins*.json`, `plugins/*/main.js`, `manifest.json`, `styles.css` | which plugins are on and their code, without their settings |
| Plugin settings | `plugins/*/data.json` | off by default: plugin settings often depend on the device (paths, keys) |
| Editor and general | `app.json`, `graph.json`, `types.json` | `app.json` also holds platform-specific options |

Window layout (`workspace*.json`) and caches never sync. selfsync's own folder with the token never syncs either, even with "Plugin settings" on. Your exclusion lines apply on top of the toggles; the resulting rules are shown under "Resulting rules".

Settings files are not merged line by line (that would break the JSON): when two devices changed the same file, the later change wins, with no conflict copy. If your version was newer, the other one stays in the server history; if the other one was newer, yours goes to the trash. Turning a toggle off only stops syncing: the files stay on the server and on other devices. Turning it back on brings over what changed meanwhile. Obsidian does not reread all its settings on the fly, so after settings arrive from another device the plugin suggests a restart (once per session).

The command-line client syncs `.obsidian/` as before (except `workspace*.json`); its `excludes` in `.selfsync/config.toml` understand `!` too.

### Light device

A phone does not have to hold everything. Two tools, both per device:

- **Download size limit.** "Do not download files larger than, MB" in the sync settings: larger files from other devices stay on the server and are not downloaded here. Files already on this device keep syncing, and files you create here are sent as usual. "Not downloaded" in the settings lists what was skipped, with a "Download" button for any single file. Raising or removing the limit downloads the rest.
- **"Do not sync on this device"** in the context menu of a folder or file adds it to this device's exclusions; nothing is deleted on the server or on other devices. After the next sync, "Free up space: delete local copies" in the same menu deletes the local files that match the server copy. Files that exist only here or were changed here are kept. "Sync on this device again" brings the folder back.

### Mobile limitations

On mobile Obsidian can only read a file whole (`readBinary`), so very large attachments need as much memory. The default file size limit is 512 MiB, but on a phone it is sensible to keep attachments much smaller. On desktop large files are read in chunks.

Argon2id for the encryption password uses 64 MiB of memory. Modern phones handle it in seconds, but unlocking once per device takes noticeable time.

### File names across platforms

Linux and macOS allow `? : * " < > |` in file names, Windows and Android storage do not (Windows also reserves `CON`, `NUL`, `COM1` and the like). Such a file syncs between the devices that can hold it; on the others it is skipped with a notice, and the rest of the vault keeps syncing. The device that creates such a name also shows a warning. Rename the file to get it everywhere.

## Command-line client

`selfsync-cli` syncs a plain folder with the same core: keep a copy of the vault on a server or NAS, or sync a machine without Obsidian.

```sh
selfsync-cli connect --dir ~/notes --server https://notes.example.com --code CODE
selfsync-cli run --dir ~/notes            # watch the folder until Ctrl-C
selfsync-cli run --dir ~/notes --once     # one cycle (cron); exit codes: 1 network, 2 password needed, 3 device revoked
selfsync-cli encrypt --dir ~/notes --password-file ~/.notes-pass
selfsync-cli rekey --dir ~/notes --password-file ~/.notes-pass
selfsync-cli status --dir ~/notes
```

History, the server trash and conflicts work without Obsidian too. Each of these commands first runs a sync cycle (it needs the current index and, for an encrypted vault, the key: `--password-file` or `SELFSYNC_PASSWORD`, as for `run`); commands that change something wait for one more cycle, so the result is already on the server when they return. Exit codes are the same as for `run --once`; `--json` prints machine-readable output.

```sh
selfsync-cli history notes/plan.md          # revisions: number, time, size, device
selfsync-cli revert notes/plan.md --rev 3   # an unsent current version is kept as a copy next to it
selfsync-cli trash                          # deleted on the server and when they expire (--all: expired ones too)
selfsync-cli restore notes/old.md
selfsync-cli purge notes/old.md --yes       # erase from the trash for good, with its history
selfsync-cli conflicts
selfsync-cli resolve 4 --keep mine          # both | mine | server
selfsync-cli skipped                        # files over max_download_size that were not downloaded
selfsync-cli download media/talk.mp4        # download one anyway
```

`max_download_size = 50000000` in `.selfsync/config.toml` (bytes, 0 — no limit) works as in the plugin.

Only one process works with a folder at a time (`.selfsync/lock`): while `run` is running as a service, stop it first (`systemctl --user stop selfsync-cli@notes`).

The client keeps its state in `<folder>/.selfsync/` (token and key with mode 0600) and puts deleted files into `<folder>/.trash/`. To run it as a service: [deploy/systemd/user/selfsync-cli@.service](deploy/systemd/user/selfsync-cli@.service), `systemctl --user enable --now selfsync-cli@notes`.

**A command after each sync.** `run --exec CMD` (or `exec = "..."` in `.selfsync/config.toml`) runs `sh -c CMD` in the folder after every cycle that changed something — commit to git, rebuild a Quartz or Hugo site, take a backup:

```sh
cd ~/notes && git init
selfsync-cli run --dir ~/notes --exec 'git add -A && git commit -qm "selfsync: $SELFSYNC_PULLED in, $SELFSYNC_PUSHED out" || true'
selfsync-cli run --dir ~/site --exec 'npx quartz build' --exec-timeout 30m
```

The hook gets `SELFSYNC_DIR`, `SELFSYNC_PULLED` (paths changed here by the sync), `SELFSYNC_PUSHED` (paths sent) and `SELFSYNC_CHANGES_FILE`: a file with all those paths, one per line (decrypted for an encrypted vault). Hooks never overlap: cycles that finish while one is running are merged into a single next run. A non-zero exit code or a timeout (`--exec-timeout`, 10 minutes by default; the whole process group is killed) is logged and does not stop syncing. With `--once`, the hook finishes before the client exits, and the exit code does not depend on it. `.git/` is excluded by default in configs created by this version; for an older config `status` warns if the folder has a `.git` that would be synced.

## Encryption

Turn on encryption from the plugin settings or with `selfsync-cli encrypt` on a vault that is already in use. Existing files are re-uploaded encrypted, then the plaintext copies on the server are erased. The server sees only ciphertext of contents and names, sizes and modification times. When encryption is turned on, the plugin also shows a **recovery phrase** of 24 words, once: write it down and keep it apart from the password. If the password is forgotten, "Forgot the password?" in the password window (or `selfsync-cli recover --phrase-file F --new-password-file P`) opens the vault with the phrase and sets a new password; the phrase stays valid. Devices that remember the key keep working, the others ask for the new password. The phrase can be replaced or removed in the plugin settings or with `selfsync-cli recovery create|remove`; `selfsync-cli encrypt --recovery` creates one when turning encryption on. A vault with a recovery phrase can be opened only by plugin and CLI 0.2.1 or newer: update every device first. The device list in the plugin settings shows which version each device runs, and before creating a phrase the plugin and CLI name the devices that are older (or whose version is unknown because they run something older than 0.3.0). Without the phrase the password cannot be recovered: the server copy is unreadable (local files on your devices stay).

**Changing the key.** Changing the password does not change the key that encrypts the vault, and a revoked device still knows that key. "Change the key" in the encryption settings (offered right after revoking a device), or `selfsync-cli rekey`, re-encrypts the whole vault with a new key under the same password; the server then erases everything encrypted with the old key, together with version history and the trash from before the change. Every device asks for the password again, a recovery phrase is replaced with a new one, and devices older than 0.3.0 stop syncing until updated (the plugin names them first). It needs server 0.3.0 or newer. If the device that started it disappears halfway, any device with the password finishes the job.

The threat model and what encryption does not hide are in [SECURITY.md](SECURITY.md) (in Russian).

## Development

```sh
cargo test --workspace                                   # core, server, client, sync scenarios
cargo run --release -p selfsync-sim -- --runs 10000      # deterministic simulation with faults
./scripts/build-wasm.sh                                  # core → plugin/pkg (1.5 MB budget)
cargo build -p selfsync-server
cd plugin && npm ci && npm run lint && npm run build && npm test   # plugin: Obsidian lint, build, e2e against the server
```

A failing simulation run reproduces by seed: `selfsync-sim --seed N --runs 1`; full trace with `SIM_FULL_TRACE=1`.

Releases: `node scripts/version.mjs set X.Y.Z`, commit, then push an annotated tag `X.Y.Z` (no `v` prefix — Obsidian requires the tag to equal the manifest version). The workflow builds everything and creates a draft release; publish it after writing the release notes.

```
crates/
  selfsync-proto   protocol schema (prost + protox, no system protoc)
  selfsync-core    sync core without I/O: paths, blobs, encryption, merge, index, engine
  selfsync-server  server: HTTP, SQLite, blobs, cgi/socket/serve modes, admin commands
  selfsync-wasm    the core for the plugin
  selfsync-cli     command-line client
  selfsync-sim     simulation: clients + the real server + faults; scenario tests
plugin/            Obsidian plugin (TypeScript, esbuild)
deploy/            systemd, Caddy, nginx, Docker, Podman
spec/              plan, architecture, protocol, decisions (in Russian)
```

License: MIT.
