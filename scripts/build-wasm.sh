#!/usr/bin/env bash
# Сборка ядра для плагина: wasm32 → wasm-bindgen (--target web) → wasm-opt -Oz.
# Результат — plugin/pkg/, его встраивает в main.js esbuild.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -p selfsync-wasm --target wasm32-unknown-unknown --profile wasm
wasm-bindgen --target web --out-dir plugin/pkg \
  target/wasm32-unknown-unknown/wasm/selfsync_wasm.wasm
wasm-opt -Oz \
  --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-mutable-globals --enable-reference-types --enable-multivalue \
  plugin/pkg/selfsync_wasm_bg.wasm -o plugin/pkg/selfsync_wasm_bg.wasm

size=$(stat -c %s plugin/pkg/selfsync_wasm_bg.wasm 2>/dev/null || stat -f %z plugin/pkg/selfsync_wasm_bg.wasm)
limit=$((1536 * 1024))
echo "selfsync_wasm_bg.wasm: $size байт (бюджет $limit)"
if [ "$size" -gt "$limit" ]; then
  echo "WASM больше бюджета 1,5 МБ" >&2
  exit 1
fi
