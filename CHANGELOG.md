# Changelog

Release notes for each version; earlier versions are described on the [releases page](https://github.com/Darkildo/selfsync/releases).

## 0.5.0

Protocol 1.4. Server, plugin and `selfsync-cli` 0.5.0 work with older counterparts as before; the new feature needs 0.5.0 on the server and on both devices.

### Encryption key over the connect code

A new device joins an encrypted vault without typing the password: the device that shows the code passes the key over an ECDH P-256 exchange. The server stores only public keys and the wrapped key, cannot read the key and cannot swap keys unnoticed.

- **Plugin.** If the key is unlocked on this device, «Connect a new device» shows a QR code that opens Obsidian directly and carries a one-time secret and a check of the exchange key (they never reach the server). When the new device connects, its name appears and the key is sent after «Give the key». With a code typed in by hand (or the server's link page), both screens show six digits, and the key is accepted only after they are confirmed on both devices. The new device always shows the digits, waits for the key and remembers it; «Enter the password instead» falls back to the old way.
- **CLI.** `selfsync-cli join` prints the link, QR code and code, waits for the new device and asks `y/N` before giving the key (with a typed code — after showing the digits). `selfsync-cli connect` takes `--link` (the server page or an `obsidian://` link from a QR code), receives the key (with a typed code — after you confirm the digits) and stores it in `.selfsync/key`. Both need a terminal; `--no-handoff` turns the exchange off, Ctrl-C stops it (exit code 130). The QR code with the secret stays in the terminal scrollback: the secret works once and gives nothing without your consent.
- **Server.** `JoinCreate.commit`, `JoinToken.handoff_commit` and `POST`/`PUT`/`GET`/`DELETE /v1/join/handoff`; a read-only device may take part in its own exchange (`PUT`, `DELETE`) and nothing else. The wrapped key is served again until the new device closes the exchange, and either side can cancel (`JoinHandoffSource.cancel`, `DELETE`) — the other one learns it on the next poll; after «Don't give» or a QR mismatch the device that showed the code offers to revoke the new device. Network errors and 5xx do not abort the exchange: clients retry with a growing pause. Exchanges live 10 minutes after the new device joins, `selfsync sweep` removes expired ones together with keys and wrappings.
- **Core and WASM.** The exchange is one module (`selfsync_core::handoff`) with a domain label; WASM exports its primitives for add-ons. Test vectors are in `spec/vectors/handoff.json`. Details: `spec/protocol.md`, «Передача ключа по коду».
