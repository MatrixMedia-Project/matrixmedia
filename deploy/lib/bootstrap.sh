# shellcheck shell=bash
: "${MM_ROOT:=/opt/mm}"

# _extract_json_field FIELD  -- read JSON on stdin, echo the first string value of
# FIELD. Minimal, jq-free (a fresh VPS may not have jq); good enough for the flat
# {"access_token":"..."} login response.
_extract_json_field() {
  grep -oE "\"$1\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -1 \
    | sed -E 's/.*:[[:space:]]*"([^"]*)"/\1/'
}

# _upsert_secret KEY VALUE  -- set KEY=VALUE in .env.secrets, replacing any existing
# line (generate_secrets writes a placeholder MM_SYNAPSE_ADMIN_TOKEN, so a plain
# append-if-absent would never overwrite it). Keeps the file mode 0600.
_upsert_secret() {
  local key="$1" val="$2" f="$MM_ROOT/.env.secrets" tmp
  mkdir -p "$MM_ROOT"; touch "$f"; chmod 600 "$f"
  tmp="$(mktemp)"
  grep -v "^${key}=" "$f" > "$tmp" || true
  printf '%s=%s\n' "$key" "$val" >> "$tmp"
  mv "$tmp" "$f"; chmod 600 "$f"
}

# capture_admin_token DOMAIN USER PASS  -- log the freshly-registered admin into
# Synapse and persist its access token as MM_SYNAPSE_ADMIN_TOKEN (upsert). The
# install flow registers the user with registration_shared_secret first, then
# calls this, then re-renders + restarts mm-core so it picks the real token up.
capture_admin_token() {
  local domain="$1" user="$2" pass="$3" resp token
  resp="$(curl -ksS -XPOST "https://matrix.$domain/_matrix/client/v3/login" \
     -d "{\"type\":\"m.login.password\",\"user\":\"$user\",\"password\":\"$pass\"}")"
  token="$(printf '%s' "$resp" | _extract_json_field access_token)"
  [ -n "$token" ] || die "could not capture admin token (login failed)"
  _upsert_secret MM_SYNAPSE_ADMIN_TOKEN "$token"
  log "captured admin token"
}

# capture_lnbits_keys -- no-op unless an LNbits service is enabled (deferred; the
# default payment path is Stripe/demo-fakestripe, which needs no runtime capture).
capture_lnbits_keys() { :; }
