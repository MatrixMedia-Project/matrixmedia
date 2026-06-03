# shellcheck shell=bash
# Requires: common.sh sourced; $MM_ROOT set (defaults /opt/mm).
: "${MM_ROOT:=/opt/mm}"
_secrets_file() { echo "$MM_ROOT/.env.secrets"; }

# gen_secret KEY [HEXLEN]  -- append KEY=<hex> if absent; never rotate.
gen_secret() {
  local key="$1" len="${2:-64}" f; f="$(_secrets_file)"
  mkdir -p "$MM_ROOT"; touch "$f"; chmod 600 "$f"
  grep -q "^${key}=" "$f" && return 0
  local val; val="$(openssl rand -hex "$((len/2))")"
  printf '%s=%s\n' "$key" "$val" >> "$f"
}
# gen_literal KEY VALUE -- append KEY=VALUE if absent (for non-random fixed values).
gen_literal() {
  local key="$1" val="$2" f; f="$(_secrets_file)"
  mkdir -p "$MM_ROOT"; touch "$f"; chmod 600 "$f"
  grep -q "^${key}=" "$f" && return 0
  printf '%s=%s\n' "$key" "$val" >> "$f"
}

# write_secret_files -- materialise the 4 Docker-secret files that the compose
# secrets: block references.  Reads values from the already-written
# $MM_ROOT/.env.secrets.  Must be called AFTER generate_secrets.
# Files are written with printf '%s' (no trailing newline) so Docker doesn't
# pass a spurious newline byte to the consuming process.
write_secret_files() {
  local sdir="$MM_ROOT/secrets"
  mkdir -p "$sdir"; chmod 700 "$sdir"
  # Source the secrets env to get the values.
  # shellcheck disable=SC1091
  local POSTGRES_APP_PASS POSTGRES_APP_ADMIN_PASS SYNAPSE_REGISTRATION_SECRET MM_SIGNUP_IP_HASH_PEPPER
  # Use grep+sed to avoid polluting the shell with all 40+ vars.
  POSTGRES_APP_PASS="$(grep     '^POSTGRES_APP_PASS='              "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  POSTGRES_APP_ADMIN_PASS="$(grep '^POSTGRES_APP_ADMIN_PASS='      "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  SYNAPSE_REGISTRATION_SECRET="$(grep '^SYNAPSE_REGISTRATION_SECRET=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  MM_SIGNUP_IP_HASH_PEPPER="$(grep '^MM_SIGNUP_IP_HASH_PEPPER='    "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"

  printf '%s' "$POSTGRES_APP_PASS"            > "$sdir/mm_db_app_password"
  printf '%s' "$POSTGRES_APP_ADMIN_PASS"      > "$sdir/mm_db_admin_password"
  printf '%s' "$SYNAPSE_REGISTRATION_SECRET"  > "$sdir/synapse_registration_shared_secret"
  printf '%s' "$MM_SIGNUP_IP_HASH_PEPPER"     > "$sdir/signup_ip_hash_pepper"

  chmod 600 "$sdir/mm_db_app_password" "$sdir/mm_db_admin_password" \
            "$sdir/synapse_registration_shared_secret" "$sdir/signup_ip_hash_pepper"
}

generate_secrets() {
  require_cmd openssl
  gen_literal LK_API_KEY "mmkey"
  gen_secret  LK_API_SECRET 64
  gen_secret  MM_AS_TOKEN 64
  gen_secret  MM_HS_TOKEN 64
  gen_secret  MM_ADMIN_TOKEN 64
  gen_secret  MM_JWT_SIGNING_KEY 64
  gen_secret  MM_SWITCH_AUTH_SECRET 64
  gen_secret  MM_SIGNUP_IP_HASH_PEPPER 64
  gen_secret  SYNAPSE_REGISTRATION_SECRET 64
  gen_secret  SYNAPSE_MACAROON_SECRET 64
  gen_secret  SYNAPSE_FORM_SECRET 64
  gen_secret  POSTGRES_SYNAPSE_PASS 32
  gen_secret  POSTGRES_APP_ADMIN_PASS 32
  gen_secret  POSTGRES_APP_PASS 32
  gen_literal MINIO_ROOT_USER "matrixmedia"
  gen_secret  MINIO_ROOT_PASSWORD 32
  gen_secret  REDIS_PASSWORD 32
  gen_secret  TURN_SECRET 64
  gen_literal TURN_USER "mm"
  gen_secret  TURN_PASS 32
  gen_secret  GRAFANA_ADMIN_PASSWORD 24
  # MM_SYNAPSE_ADMIN_TOKEN: runtime-provisioned admin token for the Synapse
  # mmbot user.  Generated here as a placeholder; deploy/up.sh overwrites it
  # after Synapse registers the mmbot user on first boot.
  gen_secret  MM_SYNAPSE_ADMIN_TOKEN 64
}
