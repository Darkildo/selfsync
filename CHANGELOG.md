# Changelog

Release notes for each version; earlier versions are described on the [releases page](https://github.com/Darkildo/selfsync/releases).

## 0.6.1

- **Plugin, fix.** The settings tab no longer shows a stale state when opened. Obsidian 1.13+ keeps the rows from `getSettingDefinitions()` and draws the stored rows when the tab opens; the plugin now asks Obsidian to recompute them on every change, even with settings closed (calls in a row are merged into one). Before, a change made while settings were closed (for example, an account or status that finished loading) was not visible until something else redrew the tab.
- **Core, fix.** If the server answers `hash_mismatch` to a small file (the body arrived different from its hash), the file no longer stops the whole upload: other files are sent, this one is retried in the next round. After 3 failures in a row for the same version the version is set aside until the file changes, with a notice naming the file and a line in the sync log.
- **Server.** Database connections of a running server stay open instead of closing after 5 minutes of idle time. Closing the last connection removed the SQLite WAL, and the first write after idle recreated it with an `fsync`; on a sleeping disk (an HDD in a USB dock) that took 8–30 s while holding the write lock, and other requests failed with `database is locked`. CGI, commands and checks still close connections after idle time.
- **Server.** The request log line carries the error code of 4xx answers (`code="…"`), and `hash_mismatch` also the number of bytes received (`detail="received=N"`).
- Protocol is unchanged.

## 0.6.0

- **Plugin.** The interface follows the language of Obsidian: English, Russian and the 70 other languages of Obsidian's list. Translations other than English and Russian are machine translations; corrections are welcome as pull requests (`plugin/src/i18n/TRANSLATING.md`). The language is picked from `getLanguage()`: the exact code (`zh-tw`, `pt-br`), then the base language, then English; a string missing in a translation is shown in English.
- **Plugin.** Dates, times and size units follow the chosen language. In Arabic, Hebrew, Persian, Urdu and Dhivehi the verification digits, the recovery phrase, addresses, tokens, codes and file paths stay left-to-right.
- **Plugin.** `main.js` grows by about 2 MB because of the translations.
- **Plugin, fix.** On Obsidian 1.13+ the settings tab redraws itself again after a connection, a password, a device change and other actions finish; before, it kept showing the old state until reopened.
- Server, protocol and `selfsync-cli` are unchanged.

## 0.5.1

- **Plugin.** On a phone, settings rows and dialogs with several buttons wrap them onto the next line instead of pushing the last one off the screen. The desktop layout is unchanged.

## 0.5.0

Protocol 1.4. Server, plugin and `selfsync-cli` 0.5.0 work with older counterparts as before; the new feature needs 0.5.0 on the server and on both devices.

### Encryption key over the connect code

A new device joins an encrypted vault without typing the password: the device that shows the code passes the key over an ECDH P-256 exchange. The server stores only public keys and the wrapped key, cannot read the key and cannot swap keys unnoticed.

- **Plugin.** If the key is unlocked on this device, «Connect a new device» shows a QR code that opens Obsidian directly and carries a one-time secret and a check of the exchange key (they never reach the server). When the new device connects, its name appears and the key is sent after «Give the key». With a code typed in by hand (or the server's link page), both screens show six digits, and the key is accepted only after they are confirmed on both devices. The new device always shows the digits, waits for the key and remembers it; «Enter the password instead» falls back to the old way.
- **CLI.** `selfsync-cli join` prints the link, QR code and code, waits for the new device and asks `y/N` before giving the key (with a typed code — after showing the digits). `selfsync-cli connect` takes `--link` (the server page or an `obsidian://` link from a QR code), receives the key (with a typed code — after you confirm the digits) and stores it in `.selfsync/key`. Both need a terminal; `--no-handoff` turns the exchange off, Ctrl-C stops it (exit code 130). The QR code with the secret stays in the terminal scrollback: the secret works once and gives nothing without your consent.
- **Server.** `JoinCreate.commit`, `JoinToken.handoff_commit` and `POST`/`PUT`/`GET`/`DELETE /v1/join/handoff`; a read-only device may take part in its own exchange (`PUT`, `DELETE`) and nothing else. The wrapped key is served again until the new device closes the exchange, and either side can cancel (`JoinHandoffSource.cancel`, `DELETE`) — the other one learns it on the next poll; after «Don't give» or a QR mismatch the device that showed the code offers to revoke the new device. Network errors and 5xx do not abort the exchange: clients retry with a growing pause. Exchanges live 10 minutes after the new device joins, `selfsync sweep` removes expired ones together with keys and wrappings.
- **Core and WASM.** The exchange is one module (`selfsync_core::handoff`) with a domain label; WASM exports its primitives for add-ons. Test vectors are in `spec/vectors/handoff.json`. Details: `spec/protocol.md`, «Передача ключа по коду».
