// Байты WASM-модуля ядра: esbuild подставляет pkg/selfsync_wasm_bg.wasm по псевдониму
// и встраивает его в main.js (loader "binary").
declare module "selfsync-wasm-bytes" {
  const bytes: Uint8Array;
  export default bytes;
}
