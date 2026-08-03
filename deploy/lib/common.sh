# shellcheck shell=bash
set -euo pipefail
MM_C_RED=$'\033[31m'; MM_C_GRN=$'\033[32m'; MM_C_YEL=$'\033[33m'; MM_C_RST=$'\033[0m'
log()  { printf '%s[mm] %s%s\n' "$MM_C_GRN" "$*" "$MM_C_RST" >&2; }
warn() { printf '%s[mm] WARN: %s%s\n' "$MM_C_YEL" "$*" "$MM_C_RST" >&2; }
die()  { printf '%s[mm] ERROR: %s%s\n' "$MM_C_RED" "$*" "$MM_C_RST" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

# profiles_from_env ENV_FILE -- compose profiles implied by the install mode.
# Demo installs enable the "demo" profile (mm-fakestripe + lnbits); real-money
# installs get NO fake payment containers.
profiles_from_env() {
  if grep -q '^MM_DEMO_MODE=true$' "${1:-/nonexistent}" 2>/dev/null; then echo demo; fi
}

# The env-file chain handed to `docker compose`, in precedence order (LAST WINS).
#
#   versions.env  — the pinned image set shipped with this release.
#   .env          — the operator's install-time config; overrides a pin if they set one.
#   .env.secrets  — generated secrets.
#
# versions.env used to be in NO chain at all: nothing sourced it, nothing passed it to
# compose. Every image pin Phase 0 added was therefore dead, and the stack fell back to the
# inline `${X_IMAGE:-default}` defaults in the compose template — which is exactly the
# "two installs a week apart run different builds" problem the pins exist to prevent.
# Sets the global array MM_ENV_FILES. Does NOT print — deliberately: returning the list
# via stdout would need `mapfile`, which is a bash-4 builtin and simply does not exist on
# the bash 3.2 that ships with macOS, where the bats suite runs.
compose_env_files() {
  MM_ENV_FILES=()
  [ -f "$MM_ROOT/versions.env" ] && MM_ENV_FILES+=(--env-file "$MM_ROOT/versions.env")
  MM_ENV_FILES+=(--env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets")
  COMPOSE_PROFILES="$(profiles_from_env "$MM_ROOT/.env")"; export COMPOSE_PROFILES
}
