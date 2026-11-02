# Translating selfsync

The plugin shows its interface in the language of the Obsidian app. Translations other than English and Russian were **machine-translated** and have not been reviewed by native speakers yet. Corrections are very welcome: open a pull request that changes the file for your language.

## Files

All files are in `plugin/src/i18n/`:

- `en.ts` — the source strings. Every key and every `{parameter}` comes from here.
- `ru.ts` — Russian, maintained by the authors. It is a useful second reference: it often says the same thing in a different way.
- `<code>.ts` — one file per language, e.g. `de.ts`, `zh-tw.ts`, `pt-br.ts`. The codes are listed in [LANGUAGES.md](LANGUAGES.md).
- `locales.ts` — the registry. A language is used only after it is added there.
- `index.ts` — language selection and helpers. Translators do not need to touch it.

If a key is missing in a translation, the English string is shown. A partial translation is fine.

## File format

```ts
// German (Deutsch). Machine translation; corrections welcome via pull request.

import type { Dict } from "./en.ts";

const de: Dict = {
  "status.idle": "Synchronisiert",
  "status.pending": "{n} ausstehend",
  // … the same keys as in en.ts, in the same order
};

export default de;
```

For codes with a dash, name the variable in camel case: `const ptBr: Dict`, `const zhTw: Dict`, `const nanTw: Dict`.

## Registering a language

In `locales.ts`, keep both lists in alphabetical order:

```ts
import de from "./de.ts";
import ptBr from "./pt-br.ts";

export const LOCALES: Record<string, Dict> = {
  de,
  "pt-br": ptBr,
  ru,
};
```

## Checking

From `plugin/`:

```sh
npm run i18n:check -- de      # one file, even before it is registered
npm run i18n:check            # every language file, with coverage
npm test                      # the same rules for every registered language
npm run typecheck             # an unknown key is also a type error
```

The check reports:

- **Errors:** an unknown key, an empty string, or a different set of `{parameters}` than in English.
- **Warnings:** a long string that is identical to English. This is often an untranslated line.

## Do not translate

- **Names:** `selfsync`, `Selfsync Cloud`, `Obsidian`, `BIP-39`, `HTTP`, `TLS`, `WASM`.
- **Parameters:** `{n}`, `{path}`, `{name}`, `{code}`, `{time}` and the like. Keep each one exactly. You may move it within the sentence.
- **Code and commands** between backticks, e.g. `` `selfsync token add` ``. The same goes for the CLI commands, config names and server settings mentioned in text: `selfsync verify`, `client_max_body_size`, `proxy_read_timeout`.
- **Codes** from the server, e.g. `read_only`.
- **Pattern syntax** in the exclude description: `/`, `*`, `**`, `!pattern`.
- **Example characters** for file names: `? : * " < > |`.

Keep `«{name}»`-style quotes around names and paths, using the quotation marks of your language (“…”, „…“, 「…」 and so on).

## Glossary

Use one translation for each term across the whole file. Where Obsidian's own translation for your language has a term, use it: [obsidian-translations](https://github.com/obsidianmd/obsidian-translations/tree/master/translations).

| English | Meaning | Russian, for reference |
| --- | --- | --- |
| vault | An Obsidian vault. Use Obsidian's word for it. | vault |
| sync, synchronisation | Sync of the vault with the server | синхронизация, синк |
| server | The user's own selfsync server | сервер |
| device | A computer or phone connected to the vault | устройство |
| one-time code / link | A code or link for connecting a new device; it works once | одноразовый код / ссылка |
| encryption password | The password that encrypts notes end to end. It is not an account password. | пароль шифрования |
| encryption key | The key derived from the password. Devices can pass it to each other. | ключ шифрования |
| recovery phrase | 24 words that replace a forgotten password | фраза восстановления |
| digits (to compare) | 6 verification digits shown on both devices during key exchange | цифры (для сверки) |
| read-only | A device that receives changes but cannot send them | только для чтения |
| conflict copy | A copy of a file kept next to it when two devices edited it | копия конфликта |
| revoke (a device) | Disconnect a device from the server | отозвать |
| trash, deleted files | Files deleted on the server, kept for a while and restorable | удалённые файлы, корзина |
| file history, revision | Earlier versions of a file kept on the server | история, версия |
| quota | The storage limit on the server | место, лимит |
| notice | An Obsidian pop-up notice | уведомление |

## Security-related strings

These strings protect users from someone tampering with the connection. **Translate them completely and literally by meaning. Do not shorten them, soften warnings or drop clauses.** If a sentence is long, keep it long.

- `handoff.*` — passing the encryption key to a new device, comparing digits, declining, revoking.
- `join.desc`, `join.nameDesc`, `join.readOnlyDesc` — a one-time link; do not show the QR code to others.
- `connect.*` — connecting a new device by code.
- `recovery.showDesc`, `recovery.copied`, `recovery.confirmFirst`, `recovery.removeDesc`, `recovery.createDesc`, `recovery.recoverDesc` — the recovery phrase.
- `encryption.*`, `rekey.*`, `password.*` — the encryption password and key.
- `notice.wrongPassword`, `notice.encryptionMismatch`, `notice.unauthorized`, `notice.serverRewound` — sync stopped for safety.

Pay special attention to the following:

- **Who acts where:** "the device that showed the code", "the new device", "this device".
- **Give the key** and **don't give the key**: these are actions with consequences.
- **"Someone may be tampering"**: keep this warning explicit.
- **"Only if the digits match"**: keep the condition.

## Style

- Short and neutral, like the Obsidian interface in your language. Use Obsidian's form of address: formal or informal "you", or an impersonal form.
- Sentence case for buttons and titles, unless your language capitalises differently.
- Keep sentence punctuation (`.`, `:`, `—`, `…`) in the natural style of your language. Keep the colon before parameters such as `{message}`.
- Numbers come as parameters, and plural forms are avoided on purpose: strings look like "conflicts: {n}". Keep that shape instead of inflecting around `{n}`.
- Right-to-left languages (ar, dv, fa, he, ur): write naturally. Parameters, codes, digits and paths are isolated as left-to-right automatically.
