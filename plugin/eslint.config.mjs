// Тот же набор правил, которым каталог плагинов Obsidian проверяет релизы.
import { defineConfig } from "eslint/config";
import obsidianmd from "eslint-plugin-obsidianmd";

export default defineConfig([
  // Тесты — код для Node (node:test), к плагину в Obsidian не относятся.
  { ignores: ["main.js", "pkg/", "node_modules/", "test/"] },
  ...obsidianmd.configs.recommended,
  {
    languageOptions: {
      parserOptions: {
        projectService: { allowDefaultProject: ["eslint.config.mjs", "esbuild.config.mjs"] },
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  // Сборка работает в Node, а не в Obsidian.
  {
    files: ["*.mjs"],
    languageOptions: { globals: { process: "readonly", console: "readonly" } },
    rules: { "obsidianmd/no-nodejs-modules": "off", "no-restricted-globals": "off", "import/no-nodejs-modules": "off" },
  },
]);
