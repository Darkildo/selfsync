// Синхронизация каталога настроек Obsidian пресетами. Каталог исключён целиком,
// пресеты повторно включают отдельные файлы (`!шаблон`), служебные файлы Obsidian
// исключены всегда, пользовательские строки — поверх всего (в том числе с `!`).

export const PRESETS = ["appearance", "hotkeys", "snippets", "pluginList", "pluginSettings", "editor"] as const;
export type Preset = (typeof PRESETS)[number];
export type ConfigSync = Record<Preset, boolean>;

export const DEFAULT_CONFIG_SYNC: ConfigSync = {
  appearance: false,
  hotkeys: false,
  snippets: false,
  pluginList: false,
  pluginSettings: false,
  editor: false,
};

/** Что включает пресет: пути относительно каталога настроек. */
export const PRESET_PATHS: Record<Preset, string[]> = {
  appearance: ["appearance.json", "themes/**"],
  hotkeys: ["hotkeys.json"],
  snippets: ["snippets/**"],
  pluginList: [
    "community-plugins.json",
    "core-plugins.json",
    "core-plugins-migration.json",
    "plugins/*/main.js",
    "plugins/*/manifest.json",
    "plugins/*/styles.css",
  ],
  // Каталог самого selfsync (токен в data.json) исключён жёстко — пресет его не вернёт.
  pluginSettings: ["plugins/*/data.json"],
  editor: ["app.json", "graph.json", "types.json"],
};

/** Не синхронизируются никогда: раскладка окон этого устройства, кэши. */
export const ALWAYS_EXCLUDED = ["workspace.json", "workspace-mobile.json", "workspaces.json", "cache/", "**/*.lock"];

/** Итоговые правила исключений для ядра. */
export function buildRules(configDir: string, presets: ConfigSync, user: string[]): string[] {
  const rules = [`${configDir}/`];
  for (const p of PRESETS) {
    if (presets[p]) for (const path of PRESET_PATHS[p]) rules.push(`!${configDir}/${path}`);
  }
  for (const path of ALWAYS_EXCLUDED) rules.push(`${configDir}/${path}`);
  return [...rules, ...user];
}

/**
 * Настройки до пресетов хранили `<configDir>/` в пользовательских исключениях.
 * Теперь эта строка — первая в итоговых правилах, а пользовательские идут после
 * пресетов: оставшись там, она снова исключила бы всё, что пресеты включили.
 */
export function migrateUserExcludes(configDir: string, excludes: string[]): string[] {
  const own = new Set([`${configDir}/`, configDir, `/${configDir}/`, `/${configDir}`]);
  return excludes.filter((l) => !own.has(l.trim()));
}

/** Путь из каталога настроек (уведомление «перезапустите Obsidian»). */
export function inConfigDir(configDir: string, path: string): boolean {
  return path.startsWith(`${configDir}/`);
}
