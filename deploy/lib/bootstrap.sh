# shellcheck shell=bash
: "${MM_ROOT:=/opt/mm}"

# _extract_json_field FIELD  -- read JSON on stdin, echo the first string value of
# FIELD. Minimal, jq-free (a fresh VPS may not have jq); good enough for the flat
# {"access_token":"..."} login response.
_extract_json_field() {
  grep -oE "\"$1\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -1 \
    | sed -E 's/.*:[[:space:]]*"([^"]*)"/\1/'
}

# _upsert_secret lives in lib/secrets.sh (the one canonical writer; rotation
# uses it too). install.sh/mmctl source secrets.sh before this file.
# generate_secrets writes a placeholder MM_SYNAPSE_ADMIN_TOKEN, so a plain
# append-if-absent would never overwrite it — hence upsert below.

# capture_admin_token DOMAIN USER PASS  -- log the freshly-registered admin into
# Synapse and persist its access token as MM_SYNAPSE_ADMIN_TOKEN (upsert). The
# install flow registers the user with registration_shared_secret first, then
# calls this, then re-renders + restarts mm-core so it picks the real token up.
capture_admin_token() {
  local domain="$1" user="$2" pass="$3" resp token
  # `|| true` on both: under install.sh's `set -euo pipefail` a failed curl or an empty
  # match exited right here, silently, instead of reaching the die that says why.
  resp="$(curl -ksS -XPOST "https://matrix.$domain/_matrix/client/v3/login" \
     -d "{\"type\":\"m.login.password\",\"user\":\"$user\",\"password\":\"$pass\"}" 2>&1)" || true
  token="$(printf '%s' "$resp" | _extract_json_field access_token || true)"
  [ -n "$token" ] || die "could not capture admin token: the owner login at https://matrix.$domain answered: $(printf '%s' "$resp" | head -c 300)"
  _upsert_secret MM_SYNAPSE_ADMIN_TOKEN "$token"
  log "captured admin token"
}

# admin_token_owner DOMAIN TOKEN -- print the Matrix ID TOKEN signs in as (whoami); non-zero
# when Synapse does not accept it, or TOKEN is empty (generate_secrets' placeholder fails too).
# The token reaches curl on stdin (-K -), never on its command line, which `ps` shows to
# every user on the host.
admin_token_owner() {
  local domain="$1" token="$2" resp uid
  [ -n "$token" ] || return 1
  resp="$(printf 'header = "Authorization: Bearer %s"\n' "$token" \
    | curl -ksS --max-time 20 -K - "https://matrix.$domain/_matrix/client/v3/account/whoami" 2>/dev/null)" || return 1
  uid="$(printf '%s' "$resp" | _extract_json_field user_id || true)"
  [ -n "$uid" ] || return 1
  printf '%s\n' "$uid"
}

# bootstrap_owner DOMAIN -- make sure mm-core has a working Synapse admin token, signing the
# owner in only when it has to.
#
# Reads ADMIN_USER / ADMIN_PASS (from --admin-user / --admin-pass, may be empty) and NONINT.
# Sets ADMIN_USER, ADMIN_PASS, GENERATED_PASS (1 = the password is one we generated) and
# OWNER_REUSED (1 = the stored token still works and was kept).
#
# A re-run used to sign in again every time, with the password generated at the first
# install. Once the owner had changed it, as they should, every re-run died on "could not
# capture admin token" while the stored token still worked. So a token Synapse still accepts
# is kept, the owner account is left alone, and nobody signs in, unless --admin-user names
# a different account.
bootstrap_owner() {
  local domain="$1" owner
  GENERATED_PASS=0; OWNER_REUSED=0
  if owner="$(admin_token_owner "$domain" "$(read_secret MM_SYNAPSE_ADMIN_TOKEN)")"; then
    local lp="${owner#@}"; lp="${lp%%:*}"
    if [ -z "$ADMIN_USER" ] || [ "$ADMIN_USER" = "$lp" ]; then
      OWNER_REUSED=1; ADMIN_USER="$lp"
      # For admin.credentials, written only if an earlier run died before writing it: then
      # the owner never saw the generated password, so it cannot have been changed.
      ADMIN_PASS="$(read_secret MM_OWNER_BOOTSTRAP_PASS)"; [ -z "$ADMIN_PASS" ] || GENERATED_PASS=1
      log "re-run: the stored admin token still works (it signs in as $owner), so the owner account is left as it is and nobody signs in"
      return 0
    fi
  fi

  # If creds weren't passed: prompt interactively, else fall back to a random admin.
  if [ -z "$ADMIN_USER" ] && [ "$NONINT" -eq 0 ]; then
    printf 'Owner username for this server (Matrix localpart, e.g. "alice"): ' >&2; read -r ADMIN_USER
  fi
  if [ -z "$ADMIN_PASS" ] && [ "$NONINT" -eq 0 ] && [ -n "$ADMIN_USER" ]; then
    printf 'Owner password (input hidden): ' >&2; read -rs ADMIN_PASS; printf '\n' >&2
  fi
  [ -n "$ADMIN_USER" ] || ADMIN_USER="admin"
  if [ -z "$ADMIN_PASS" ]; then
    # Converge on re-runs: reuse the password we generated last time, otherwise
    # capture_admin_token dies on "admin exists with a different password".
    ADMIN_PASS="$(read_secret MM_OWNER_BOOTSTRAP_PASS)"
    if [ -n "$ADMIN_PASS" ]; then GENERATED_PASS=1; else
      ADMIN_PASS="$(openssl rand -hex 12)"; GENERATED_PASS=1
      _upsert_secret MM_OWNER_BOOTSTRAP_PASS "$ADMIN_PASS"
    fi
  fi
  # Validate the localpart so register_new_matrix_user doesn't fail cryptically.
  printf '%s' "$ADMIN_USER" | grep -qE '^[a-z0-9._=/-]+$' || die "admin user must be a valid Matrix localpart (lowercase a-z 0-9 . _ = / -)"

  docker exec matrixmedia-synapse-1 register_new_matrix_user -c /data/homeserver.yaml \
     -u "$ADMIN_USER" -p "$ADMIN_PASS" -a || warn "owner account may already exist"
  capture_admin_token "$domain" "$ADMIN_USER" "$ADMIN_PASS"
}

# capture_lnbits_keys -- no-op unless an LNbits service is enabled (deferred; the
# default payment path is Stripe/demo-fakestripe, which needs no runtime capture).
capture_lnbits_keys() { :; }
