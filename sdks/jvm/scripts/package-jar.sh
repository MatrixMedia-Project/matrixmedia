#!/usr/bin/env bash
#
# package-jar.sh — collect cross-built native libraries from
# `target/<triple>/release/` and lay them into the Kotlin module's
# `src/main/resources/<jna-resource-prefix>/` so the resulting JAR is
# self-contained.
#
# JNA resource prefix convention (used by uniffi-generated Kotlin):
#   linux-x86-64       -> libmatrix_mm_ffi.so
#   linux-aarch64      -> libmatrix_mm_ffi.so
#   win32-x86-64       -> matrix_mm_ffi.dll
#   darwin-x86-64      -> libmatrix_mm_ffi.dylib
#   darwin-aarch64     -> libmatrix_mm_ffi.dylib
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JVM_SDK_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RES_DIR="$JVM_SDK_DIR/kotlin/src/main/resources"

# (rust-triple, jna-prefix, lib-filename)
MAPPINGS=(
  "x86_64-pc-windows-msvc:win32-x86-64:matrix_mm_ffi.dll"
  "x86_64-unknown-linux-gnu:linux-x86-64:libmatrix_mm_ffi.so"
  "aarch64-unknown-linux-gnu:linux-aarch64:libmatrix_mm_ffi.so"
  # TODO(rust-dev): add darwin entries once macOS targets are enabled in
  # build-all.sh.
)

copy_one() {
  local entry="$1"
  IFS=':' read -r triple prefix filename <<<"$entry"

  local src="$JVM_SDK_DIR/target/$triple/release/$filename"
  local dst_dir="$RES_DIR/$prefix"
  local dst="$dst_dir/$filename"

  if [[ ! -f "$src" ]]; then
    echo "WARN: missing $src — skipping (did build-all.sh run?)" >&2
    return 0
  fi

  mkdir -p "$dst_dir"
  cp -v "$src" "$dst"
}

main() {
  echo "==> Packaging native libs into $RES_DIR"
  for entry in "${MAPPINGS[@]}"; do
    copy_one "$entry"
  done

  # TODO(rust-dev):
  #   - run `strip`/`llvm-strip` on Linux/macOS artifacts to shrink JAR size
  #   - optionally re-sign Windows .dll if a signing cert is configured
  #   - then `cd kotlin && ./gradlew jar` to produce the final fat .jar
  echo "==> Done. Now run: (cd kotlin && ./gradlew jar)"
}

main "$@"
