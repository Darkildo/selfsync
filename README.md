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
sudo cp deploy/systemd/selfsync.{socket,service} deploy/systemd/selfsync-sweep.{service,timer} /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now selfsync.socket selfsync-sweep.timer
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

### Admin commands

```sh
selfsync token add --vault notes --name laptop   # device token (the vault is created on first use)
selfsync link --vault notes --name phone         # one-time link + QR code in the terminal
selfsync token list | revoke --name phone
selfsync vault list
selfsync import --vault notes --from ~/Notes     # upload an existing folder
selfsync backup /backups/selfsync-$(date +%F)    # consistent copy while running
selfsync gc --vault notes                         # garbage collection plan; --yes to run it
selfsync sweep                                    # erase deleted files older than the retention window (timer)
selfsync healthcheck
```

Back up with `backup`: the copy is consistent even while the server is running. To restore, put the directory back. Devices notice that the server went back in time and run a full reconciliation without deleting anything.

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

The vault's configuration folder (`.obsidian/` by default; settings differ between devices) and `.trash/` are not synced by default. Exclusions are configurable.

### Mobile limitations

On mobile Obsidian can only read a file whole (`readBinary`), so very large attachments need as much memory. The default file size limit is 512 MiB, but on a phone it is sensible to keep attachments much smaller. On desktop large files are read in chunks.

Argon2id for the encryption password uses 64 MiB of memory. Modern phones handle it in seconds, but unlocking once per device takes noticeable time.

## Command-line client

`selfsync-cli` syncs a plain folder with the same core: keep a copy of the vault on a server or NAS, or sync a machine without Obsidian.

```sh
selfsync-cli connect --dir ~/notes --server https://notes.example.com --code CODE
selfsync-cli run --dir ~/notes            # watch the folder until Ctrl-C
selfsync-cli run --dir ~/notes --once     # one cycle (cron); exit codes: 1 network, 2 password needed, 3 device revoked
selfsync-cli encrypt --dir ~/notes --password-file ~/.notes-pass
selfsync-cli status --dir ~/notes
```

The client keeps its state in `<folder>/.selfsync/` (token and key with mode 0600) and puts deleted files into `<folder>/.trash/`. To run it as a service: [deploy/systemd/user/selfsync-cli@.service](deploy/systemd/user/selfsync-cli@.service), `systemctl --user enable --now selfsync-cli@notes`.

## Encryption

Turn on encryption from the plugin settings or with `selfsync-cli encrypt` on a vault that is already in use. Existing files are re-uploaded encrypted, then the plaintext copies on the server are erased. The server sees only ciphertext of contents and names, sizes and modification times. The password cannot be recovered: without it the server copy is unreadable (local files on your devices stay). The threat model and what encryption does not hide are in [SECURITY.md](SECURITY.md) (in Russian).

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
