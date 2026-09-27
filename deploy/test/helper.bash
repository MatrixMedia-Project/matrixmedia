# Sourced by every .bats file. Resolves the deploy/ root and a temp HOME.
DEPLOY_ROOT="$(cd "$(dirname "$BATS_TEST_FILENAME")/.." && pwd)"
setup_tmp() { MM_ROOT="$(mktemp -d)"; export MM_ROOT; }
teardown_tmp() { [ -n "${MM_ROOT:-}" ] && rm -rf "$MM_ROOT"; }

# Octal permission bits of a file ("600"), on Linux AND macOS.
#
# GNU first, on purpose. The tests used `stat -f '%Lp' F || stat -c '%a' F` — BSD
# syntax first — which is wrong on Linux in a way that never shows up on a Mac: GNU
# `stat -f` means *filesystem* status, so it prints a block of filesystem info, fails on
# the `%Lp` operand, and the fallback then appends "600". The comparison saw both and
# failed on every CI run (ubuntu), while passing on every developer laptop (macOS).
# `stat -c` on BSD fails with nothing on stdout, so this order is clean on both.
file_mode() { stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"; }

