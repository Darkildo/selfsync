#!/usr/bin/env bash
# Сборка ядра для плагина: wasm32 → wasm-bindgen (--target web) → wasm-opt -Oz.
# Результат — plugin/pkg/, его встраивает в main.js esbuild.
set -euo pipefail
cd "$(dirname "$0")/.."

# Пути машины (реестр cargo, тулчейн, каталог сборки) не попадают в WASM: сборка
# одного коммита одинакова на любой машине. С компонентом rust-src пути std ведут
# в его исходники, без него — на виртуальный /rustc/<коммит>: приводим к нему
# (из нескольких подходящих правил действует последнее).
sysroot=$(rustc --print sysroot)
commit=$(rustc -vV | sed -n 's/^commit-hash: //p')
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}/registry/src=/cargo \
--remap-path-prefix=$sysroot=/rust --remap-path-prefix=$sysroot/lib/rustlib/src/rust=/rustc/$commit \
--remap-path-prefix=$PWD=/selfsync"

cargo build -p selfsync-wasm --target wasm32-unknown-unknown --profile wasm
if grep -aq -e "$HOME" -e "$PWD" target/wasm32-unknown-unknown/wasm/selfsync_wasm.wasm; then
  echo "в WASM остались пути этой машины" >&2
  exit 1
fi
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
