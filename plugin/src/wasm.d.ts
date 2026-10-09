// Байты WASM-модуля ядра: esbuild подставляет pkg/notesync_wasm_bg.wasm по псевдониму
// и встраивает его в main.js (loader "binary").
declare module "notesync-wasm-bytes" {
  const bytes: Uint8Array;
  export default bytes;
}
