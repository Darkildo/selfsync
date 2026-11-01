# Changelog

Release notes for each version; earlier versions are described on the [releases page](https://github.com/Darkildo/selfsync/releases).

## 0.5.0

Protocol 1.4. Server, plugin and `selfsync-cli` 0.5.0 work with older counterparts as before; the new feature needs 0.5.0 on the server and on both devices.

### Encryption key over the connect code

A new device joins an encrypted vault without typing the password: the device that shows the code passes the key over an ECDH P-256 exchange. The server stores only public keys and the wrapped key, cannot read the key and cannot swap keys unnoticed.

- **Plugin.** If the key is unlocked on this device, «Connect a new device» shows a QR code that opens Obsidian directly and carries a one-time secret (it never reaches the server). When the new device connects, its name appears and the key is sent after «Give the key». With a code typed in by hand (or the server's link page), both screens show six digits to compare first. The new device waits for the key and remembers it; «Enter the password instead» falls back to the old way.
- **CLI.** `selfsync-cli join` prints the link, QR code and code, waits for the new device and asks `y/N` before giving the key (with a typed code — after showing the digits). `selfsync-cli connect` takes `--link` (the server page or an `obsidian://` link from a QR code), receives the key and stores it in `.selfsync/key`. Both need a terminal; `--no-handoff` turns the exchange off.
- **Server.** `JoinCreate.commit`, `JoinToken.handoff_commit` and `POST`/`PUT`/`GET /v1/join/handoff`; a read-only device may send its exchange key (`PUT`) and nothing else. Exchanges live 10 minutes after the new device joins, `selfsync sweep` removes expired ones together with keys and wrappings.
- **Core and WASM.** The exchange is one module (`selfsync_core::handoff`) with a domain label; WASM exports its primitives for add-ons. Test vectors are in `spec/vectors/handoff.json`. Details: `spec/protocol.md`, «Передача ключа по коду».
