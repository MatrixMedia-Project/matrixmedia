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
