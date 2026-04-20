#!/usr/bin/env bash
#
# build-all.sh — cross-compile the matrix-mm-ffi crate for every supported
# desktop JVM triple. Uses `cargo zigbuild` so we can produce Linux + Windows
# binaries from any host (per the project-wide convention; see
# feedback_zigbuild.md in the user's memory).
#
# Prerequisites (see docs/BUILD.md for installation steps):
#   - Rust toolchain (rustup) with the relevant `rustup target add ...` done
#   - zig (>= 0.13)
#   - cargo-zigbuild (`cargo install cargo-zigbuild`)
#
# Usage:
#   ./scripts/build-all.sh                # build all triples
#   ./scripts/build-all.sh linux-x64      # build a single named triple
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JVM_SDK_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$JVM_SDK_DIR"

# Map of friendly-name -> rust target triple.
# Keep in sync with kotlin/build.gradle.kts `supportedTriples` and with
# docs/BUILD.md.
declare -A TRIPLES=(
  [windows-x64]="x86_64-pc-windows-msvc"
  [linux-x64]="x86_64-unknown-linux-gnu"
  [linux-arm64]="aarch64-unknown-linux-gnu"
  # TODO(rust-dev): add macOS triples once we have a signing story.
  # [macos-x64]="x86_64-apple-darwin"
  # [macos-arm64]="aarch64-apple-darwin"
)

build_one() {
  local friendly="$1"
  local triple="${TRIPLES[$friendly]:-}"
  if [[ -z "$triple" ]]; then
    echo "ERROR: unknown target '$friendly'. Known: ${!TRIPLES[*]}" >&2
    return 1
  fi

  echo "==> Building matrix-mm-ffi for $friendly ($triple)"

  # TODO(rust-dev): some triples need extra flags / sysroots. Examples:
  #   - windows-msvc requires `xwin` set up so zigbuild can find the SDK
  #   - linux-gnu picks a glibc version via `--target ...gnu.2.17` for
  #     better backwards compat
  #   - macos targets need `SDKROOT` / `MACOSX_DEPLOYMENT_TARGET` set
  cargo zigbuild --release --target "$triple" -p matrix-mm-ffi
}

main() {
  if [[ $# -eq 0 ]]; then
    for friendly in "${!TRIPLES[@]}"; do
      build_one "$friendly"
    done
  else
    for friendly in "$@"; do
      build_one "$friendly"
    done
  fi

  echo
  echo "==> Done. Artifacts under target/<triple>/release/"
  echo "    Run scripts/package-jar.sh next to lay them into the Kotlin module."
  # TODO(rust-dev): optionally `exec "$SCRIPT_DIR/package-jar.sh"` here once
  # that script is implemented.
}

main "$@"
