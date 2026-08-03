# shellcheck shell=bash
set -euo pipefail
MM_C_RED=$'\033[31m'; MM_C_GRN=$'\033[32m'; MM_C_YEL=$'\033[33m'; MM_C_RST=$'\033[0m'
log()  { printf '%s[mm] %s%s\n' "$MM_C_GRN" "$*" "$MM_C_RST" >&2; }
warn() { printf '%s[mm] WARN: %s%s\n' "$MM_C_YEL" "$*" "$MM_C_RST" >&2; }
die()  { printf '%s[mm] ERROR: %s%s\n' "$MM_C_RED" "$*" "$MM_C_RST" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }
