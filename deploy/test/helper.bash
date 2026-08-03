# Sourced by every .bats file. Resolves the deploy/ root and a temp HOME.
DEPLOY_ROOT="$(cd "$(dirname "$BATS_TEST_FILENAME")/.." && pwd)"
setup_tmp() { MM_ROOT="$(mktemp -d)"; export MM_ROOT; }
teardown_tmp() { [ -n "${MM_ROOT:-}" ] && rm -rf "$MM_ROOT"; }
