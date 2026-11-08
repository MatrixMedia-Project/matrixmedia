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

# mm-core's own Synapse admin account. Its token is MM_SYNAPSE_ADMIN_TOKEN; its password,
# generated here and never shown to anyone, is MM_SYNAPSE_SERVICE_PASS.
#
# The token used to be a session of the OWNER's account. A Matrix password change logs the
# user's other devices out by default (logout_devices), so an owner who changed the
# generated password revoked mm-core's token: the Operator Console then signed them in as
# "demo" (mm-core decides admin with this token), membership gates and moderation failed
# closed, and a re-run could not sign in again. Everything mm-core does with the token is a
# Synapse admin API call, so any admin account serves; this one belongs to no person.
MM_SERVICE_USER="mm-core"

# _json_str S -- S escaped for use inside a JSON string (backslash, double quote).
_json_str() { local v="${1//\\/\\\\}"; printf '%s' "${v//\"/\\\"}"; }

# _synapse_login DOMAIN USER PASS -- print Synapse's answer to a password login. The JSON
# body reaches curl on stdin (-d @-), never on its command line, which `ps` shows to every
# user on the host.
_synapse_login() {
  printf '{"type":"m.login.password","user":"%s","password":"%s"}' "$(_json_str "$2")" "$(_json_str "$3")" \
    | curl -ksS --max-time 30 -XPOST "https://matrix.$1/_matrix/client/v3/login" -d @- 2>&1
}

# _synapse_logout DOMAIN TOKEN -- end the session TOKEN belongs to (best effort).
_synapse_logout() {
  printf 'header = "Authorization: Bearer %s"\n' "$2" \
    | curl -ksS --max-time 20 -K - -XPOST "https://matrix.$1/_matrix/client/v3/logout" -d '{}' >/dev/null 2>&1 || true
}

# capture_admin_token DOMAIN USER PASS -- sign USER in and persist the access token as
# MM_SYNAPSE_ADMIN_TOKEN (upsert; generate_secrets writes a placeholder, so a plain
# append-if-absent would never overwrite it). install.sh re-renders and restarts mm-core
# afterwards so it picks the token up.
capture_admin_token() {
  local domain="$1" user="$2" pass="$3" resp token
  # `|| true` on both: under install.sh's `set -euo pipefail` a failed curl or an empty
  # match exited right here, silently, instead of reaching the die that says why.
  resp="$(_synapse_login "$domain" "$user" "$pass")" || true
  token="$(printf '%s' "$resp" | _extract_json_field access_token || true)"
  [ -n "$token" ] || die "could not capture admin token: signing in @$user:$domain at https://matrix.$domain answered: $(printf '%s' "$resp" | head -c 300)"
  _upsert_secret MM_SYNAPSE_ADMIN_TOKEN "$token"
  log "captured admin token (@$user:$domain)"
}

# admin_token_owner DOMAIN TOKEN -- print the Matrix ID TOKEN signs in as (whoami); non-zero
# when Synapse does not accept it, or TOKEN is empty. The token goes to curl on stdin (-K -).
admin_token_owner() {
  local domain="$1" token="$2" resp uid
  [ -n "$token" ] || return 1
  resp="$(printf 'header = "Authorization: Bearer %s"\n' "$token" \
    | curl -ksS --max-time 20 -K - "https://matrix.$domain/_matrix/client/v3/account/whoami" 2>/dev/null)" || return 1
  uid="$(printf '%s' "$resp" | _extract_json_field user_id || true)"
  [ -n "$uid" ] || return 1
  printf '%s\n' "$uid"
}

# Synapse's shared-secret registration (/_synapse/admin/v1/register), as
# register_new_matrix_user does it, plus inhibit_login. Without that, Synapse also signs the
# new account in, and register_new_matrix_user (which cannot ask for it) threw that
# session's token away: every install left an unused session on each account it created,
# listed among the owner's sessions in their Matrix app. Run by _register_user with
# Synapse's own Python; standard library only. argv: USER CONFIG URL; the password on stdin.
# The secret is read from CONFIG's registration_shared_secret line, which the installer
# renders (homeserver.tmpl.yaml).
MM_REGISTER_PY='
import hashlib, hmac, json, re, sys, urllib.error, urllib.request
user, config, url = sys.argv[1:4]
password = sys.stdin.buffer.read().decode("utf-8")
with open(config, encoding="utf-8") as f:
    m = re.search(r"^registration_shared_secret:[ \t]*[\"\x27]?([^\"\x27\s]+)", f.read(), re.M)
if not m:
    sys.exit("no registration_shared_secret in " + config)

def call(body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url + "/_synapse/admin/v1/register", data=data,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        sys.exit("Synapse answered %d: %s" % (e.code, e.read().decode("utf-8", "replace")[:300]))
    except (OSError, ValueError) as e:
        sys.exit("Synapse at %s did not answer: %s" % (url, e))

nonce = call().get("nonce", "")
mac = hmac.new(m.group(1).encode(), "\0".join([nonce, user, password, "admin"]).encode("utf-8"),
               hashlib.sha1).hexdigest()
call({"nonce": nonce, "username": user, "password": password, "admin": True, "mac": mac,
      "inhibit_login": True})
'

# _register_user USER PASS -- create USER as a Synapse admin, with no session (MM_REGISTER_PY),
# inside Synapse's container: its Python, its config, its listener. The password reaches it
# on stdin, never on a command line, which `ps` shows to every user on the host. Non-zero,
# with Synapse's answer on stderr, if the account exists already.
_register_user() {
  printf '%s' "$2" | docker exec -i matrixmedia-synapse-1 python3 -c "$MM_REGISTER_PY" \
    "$1" /data/homeserver.yaml http://localhost:8008
}

# service_admin_token DOMAIN -- make sure MM_SYNAPSE_ADMIN_TOKEN is a working token of
# mm-core's own account: keep it while it works, otherwise (create the account and) sign it
# in with MM_SYNAPSE_SERVICE_PASS. An install from before this held a session of the owner's
# account; it is handed to retire_old_admin_token through MM_RETIRED_ADMIN_TOKEN, to sign
# out once mm-core runs on the new token.
service_admin_token() {
  local domain="$1" old owner pass svc="@$MM_SERVICE_USER:$1"
  old="$(read_secret MM_SYNAPSE_ADMIN_TOKEN)"
  owner="$(admin_token_owner "$domain" "$old" || true)"
  if [ "$owner" = "$svc" ]; then
    log "kept mm-core's Synapse admin token ($svc; it still works)"
    return 0
  fi
  pass="$(read_secret MM_SYNAPSE_SERVICE_PASS)"
  if [ -z "$pass" ]; then
    pass="$(openssl rand -hex 24)"
    _upsert_secret MM_SYNAPSE_SERVICE_PASS "$pass"
  fi
  # On a re-run the account exists and this fails; the sign-in below says whether it matters.
  _register_user "$MM_SERVICE_USER" "$pass" >/dev/null 2>&1 || true
  capture_admin_token "$domain" "$MM_SERVICE_USER" "$pass"
  if [ -n "$owner" ]; then
    MM_RETIRED_ADMIN_TOKEN="$old"; MM_RETIRED_ADMIN_OWNER="$owner"
    log "mm-core now uses its own Synapse account $svc instead of a session of $owner"
  fi
}

# retire_old_admin_token DOMAIN -- sign out the owner session service_admin_token replaced:
# it is a live admin token, and backups taken before still hold it. Only call this once
# mm-core runs on the new token: until then the running mm-core still uses the old one, and
# signing it out earlier left mm-core on a revoked token whenever a step in between failed.
retire_old_admin_token() {
  [ -n "${MM_RETIRED_ADMIN_TOKEN:-}" ] || return 0
  _synapse_logout "$1" "$MM_RETIRED_ADMIN_TOKEN"
  log "signed out the installer's old session of ${MM_RETIRED_ADMIN_OWNER:-the owner}"
  MM_RETIRED_ADMIN_TOKEN=""
}

# owner_signs_in DOMAIN USER PASS -- 0 when the owner account signs in with PASS (that
# session is signed straight out again); else die with Synapse's answer. The Operator Console
# signs in the same way, so an owner who cannot is locked out of their own server.
owner_signs_in() {
  local domain="$1" user="$2" pass="$3" resp token
  resp="$(_synapse_login "$domain" "$user" "$pass")" || true
  token="$(printf '%s' "$resp" | _extract_json_field access_token || true)"
  [ -n "$token" ] || die "the owner account @$user:$domain cannot sign in: https://matrix.$domain answered: $(printf '%s' "$resp" | head -c 300). If it already exists with another password, re-run with --admin-pass and its current password."
  _synapse_logout "$domain" "$token"
}

# bootstrap_owner DOMAIN -- mm-core's admin token (service_admin_token), then the OWNER's
# account: the person who signs in to the Operator Console with the username and password
# they choose.
#
# Reads ADMIN_USER / ADMIN_PASS (from --admin-user / --admin-pass, may be empty) and NONINT.
# Sets ADMIN_USER, ADMIN_PASS, GENERATED_PASS (1 = the password is one we generated) and
# OWNER_REUSED (1 = a re-run that left the owner account as it was).
#
# The owner account is created on the first install. A re-run of a completed install leaves
# it alone, password included, unless --admin-user or --admin-pass is passed.
bootstrap_owner() {
  local domain="$1"
  GENERATED_PASS=0; OWNER_REUSED=0
  service_admin_token "$domain"

  # admin.credentials is the last thing a completed install writes.
  if [ -f "$MM_ROOT/admin.credentials" ] && [ -z "$ADMIN_USER$ADMIN_PASS" ]; then
    OWNER_REUSED=1
    ADMIN_USER="$(sed -n 's/^owner_user=//p' "$MM_ROOT/admin.credentials" | head -1)"
    [ -n "$ADMIN_USER" ] || ADMIN_USER="admin"
    log "re-run: the owner account @$ADMIN_USER:$domain is left as it is"
    return 0
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
    # A half-finished first install that is run again reuses the password it generated:
    # the account may already exist with it.
    ADMIN_PASS="$(read_secret MM_OWNER_BOOTSTRAP_PASS)"
    if [ -n "$ADMIN_PASS" ]; then GENERATED_PASS=1; else
      ADMIN_PASS="$(openssl rand -hex 12)"; GENERATED_PASS=1
      _upsert_secret MM_OWNER_BOOTSTRAP_PASS "$ADMIN_PASS"
    fi
  fi
  # Validate the localpart so registration doesn't fail cryptically.
  printf '%s' "$ADMIN_USER" | grep -qE '^[a-z0-9._=/-]+$' || die "admin user must be a valid Matrix localpart (lowercase a-z 0-9 . _ = / -)"
  [ "$ADMIN_USER" != "$MM_SERVICE_USER" ] || die "\"$MM_SERVICE_USER\" is mm-core's own account; choose another owner name"

  _register_user "$ADMIN_USER" "$ADMIN_PASS" || warn "owner account may already exist"
  owner_signs_in "$domain" "$ADMIN_USER" "$ADMIN_PASS"
}

# capture_lnbits_keys -- no-op unless an LNbits service is enabled (deferred; the
# default payment path is Stripe/demo-fakestripe, which needs no runtime capture).
capture_lnbits_keys() { :; }
