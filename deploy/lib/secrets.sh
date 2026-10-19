# shellcheck shell=bash
# Requires: common.sh sourced; $MM_ROOT set (defaults /opt/mm).
: "${MM_ROOT:=/opt/mm}"
_secrets_file() { echo "$MM_ROOT/.env.secrets"; }

# gen_secret KEY [HEXLEN]  -- append KEY=<hex> if absent; never rotate.
# Non-zero, writing nothing, when openssl fails: an empty KEY= line would count as
# present on every later run and never be filled in.
gen_secret() {
  local key="$1" len="${2:-64}" f; f="$(_secrets_file)"
  mkdir -p "$MM_ROOT"; touch "$f"; chmod 600 "$f"
  grep -q "^${key}=" "$f" && return 0
  local val; val="$(openssl rand -hex "$((len/2))")" || return 1
  [ -n "$val" ] || return 1
  printf '%s=%s\n' "$key" "$val" >> "$f"
}
# gen_literal KEY VALUE -- append KEY=VALUE if absent (for non-random fixed values).
gen_literal() {
  local key="$1" val="$2" f; f="$(_secrets_file)"
  mkdir -p "$MM_ROOT"; touch "$f"; chmod 600 "$f"
  grep -q "^${key}=" "$f" && return 0
  printf '%s=%s\n' "$key" "$val" >> "$f"
}

# read_secret KEY [FILE] -- echo KEY's value from .env.secrets ("" if absent).
# pipefail-safe: a missing key must not kill a set -euo pipefail caller.
read_secret() {
  local key="$1" file="${2:-$MM_ROOT/.env.secrets}"
  grep -s "^${key}=" "$file" | cut -d= -f2- | head -1 || true
}

# _secrets_copy_without KEY FILE TMP -- write FILE minus its KEY= line into TMP
# (mode 0600). grep exit 1 only means "no line left" and is fine; anything worse
# (FILE unreadable, disk full) removes TMP and dies, so a failed read can never be
# written back over FILE as an empty file.
_secrets_copy_without() {
  local key="$1" f="$2" tmp="$3" rc=0
  chmod 600 "$tmp"
  grep -v "^${key}=" "$f" > "$tmp" || rc=$?
  if [ "$rc" -gt 1 ]; then
    rm -f "$tmp"
    die "cannot read $f (grep exit $rc); left unchanged"
  fi
}

# _upsert_secret KEY VALUE -- set KEY=VALUE in .env.secrets, replacing any
# existing line. The single canonical WRITER for rotation (lib/rotate.sh) and
# runtime capture (lib/bootstrap.sh); gen_secret/gen_literal stay append-if-
# absent by contract. Keeps the file mode 0600. The temp file sits next to the
# target, so the final mv is an atomic rename on the same filesystem.
_upsert_secret() {
  local key="$1" val="$2" f tmp; f="$(_secrets_file)"
  mkdir -p "$MM_ROOT"
  [ -e "$f" ] || { touch "$f"; chmod 600 "$f"; }
  tmp="$(mktemp "$f.XXXXXX")" || die "cannot create a temp file next to $f; left unchanged"
  _secrets_copy_without "$key" "$f" "$tmp"
  printf '%s=%s\n' "$key" "$val" >> "$tmp"
  mv "$tmp" "$f"; chmod 600 "$f"
}

# _remove_secret KEY -- delete KEY from .env.secrets (mode 0600 kept; atomic rename).
_remove_secret() {
  local key="$1" f tmp; f="$(_secrets_file)"
  [ -f "$f" ] || return 0
  tmp="$(mktemp "$f.XXXXXX")" || die "cannot create a temp file next to $f; left unchanged"
  _secrets_copy_without "$key" "$f" "$tmp"
  mv "$tmp" "$f"; chmod 600 "$f"
}

# compose_passes_settings_key -- true when the rendered compose file hands
# MM_SETTINGS_ENCRYPTION_KEY to mm-core (files rendered before it existed don't).
compose_passes_settings_key() {
  grep -q 'MM_SETTINGS_ENCRYPTION_KEY:' "$MM_ROOT/docker-compose.yml" 2>/dev/null
}

# compose_passes_alert_token -- true when the rendered compose file hands
# MM_ALERT_WEBHOOK_TOKEN to mm-core (a non-comment line; files rendered before it existed
# don't, so a token set in .env.secrets would never reach the container).
compose_passes_alert_token() {
  grep -qE '^[[:space:]]*MM_ALERT_WEBHOOK_TOKEN:' "$MM_ROOT/docker-compose.yml" 2>/dev/null
}

# compose_mounts_ad_incoming -- true when the rendered compose file gives mm-core the ad upload
# directory (a non-comment mount line). Files rendered before ad conversion don't: mm-core then
# has no /mm-ad-media/incoming to write to and every ad upload fails with a 500.
compose_mounts_ad_incoming() {
  grep -qE '^[[:space:]]*-[[:space:]].*ad-media/incoming:/mm-ad-media/incoming' "$MM_ROOT/docker-compose.yml" 2>/dev/null
}

# compose_passes_settings_previous_key -- true when the rendered compose file also hands
# MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS to mm-core (a non-comment line). A key rotation
# needs it: without it mm-core boots with the new key only, and every secret still
# under the old key is lost once _PREVIOUS is dropped.
compose_passes_settings_previous_key() {
  grep -qE '^[[:space:]]*MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS:' "$MM_ROOT/docker-compose.yml" 2>/dev/null
}

# File permission model ("more secure approach", cutover decision B2):
#
#   $MM_ROOT/secrets/<compose secret file>   root:matrixmedia(101), 0640.
#     The containers read these via the group: mm-core runs as uid 100 / gid
#     101 (infra/docker/Dockerfile) and consumes the synapse/pepper secrets,
#     and the mm-postgres entrypoint (root at startup) reads the db passwords.
#     Nobody else on the host gets anything: no other-user bits, and the
#     secrets dir itself is 0700. chown is numeric (0:101) so it works when the
#     host has no `matrixmedia` group entry.
#   0600 where group read is unnecessary: .env.secrets, admin.credentials and
#     the rendered config/mm-postgres-init.sql (which embeds both db passwords)
#     are host-root-only; no container reads them via the group.
#
# Enforcement is idempotent (ownership+mode only, content never rewritten) and
# every enforcement point is followed by assert_secret_perms, which FAILS CLOSED:
# a secret file left world-readable aborts the install instead of starting an
# exposed stack.
_secret_chown() {
  # Best-effort outside root (bats runs non-root and cannot chown); strict under
  # it — a failed chown as root exits non-zero and the caller dies.
  if [ "$(id -u)" -eq 0 ]; then chown 0:101 "$@"; else true; fi
}

# enforce_secret_perms -- apply the model above to every present secret-bearing
# file. Missing files are skipped (a pre-generate state is not an error).
enforce_secret_perms() {
  local sdir="$MM_ROOT/secrets" f
  [ -d "$sdir" ] || return 0
  chmod 700 "$sdir"
  # Plain ifs, not `&&` lists: under callers' `set -e` a missing optional file
  # must skip, not fail the whole enforcement run.
  if [ -f "$MM_ROOT/.env.secrets" ]; then chmod 600 "$MM_ROOT/.env.secrets"; fi
  for f in mm_db_app_password mm_db_admin_password synapse_registration_shared_secret signup_ip_hash_pepper; do
    [ -f "$sdir/$f" ] || continue
    _secret_chown "$sdir/$f"
    chmod 640 "$sdir/$f"
  done
  if [ -f "$MM_ROOT/admin.credentials" ]; then chmod 600 "$MM_ROOT/admin.credentials"; fi
  if [ -f "$MM_ROOT/config/mm-postgres-init.sql" ]; then chmod 600 "$MM_ROOT/config/mm-postgres-init.sql"; fi
}

# _secret_world_readable FILE -- 0 when FILE grants anything to other (o+rwx).
_secret_world_readable() {
  local mode other
  mode="$(stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1")"
  other=$(( 8#$mode & 7 ))
  [ "$other" -ne 0 ]
}

# assert_secret_perms -- fail CLOSED: die listing every present secret file that
# is world-readable. A chmod that silently failed (read-only mount, ACL, wrong
# user) must abort the install, never leave secrets readable.
assert_secret_perms() {
  local bad="" f
  for f in "$MM_ROOT/.env.secrets" \
           "$MM_ROOT/secrets/mm_db_app_password" \
           "$MM_ROOT/secrets/mm_db_admin_password" \
           "$MM_ROOT/secrets/synapse_registration_shared_secret" \
           "$MM_ROOT/secrets/signup_ip_hash_pepper" \
           "$MM_ROOT/admin.credentials" \
           "$MM_ROOT/config/mm-postgres-init.sql"; do
    [ -f "$f" ] || continue
    if _secret_world_readable "$f"; then bad="$bad $f"; fi
  done
  [ -z "$bad" ] || die "refusing to continue: world-readable secret files:$bad"
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

  # The permission model, applied by the same idempotent path as every re-run.
  _secret_chown "$sdir/mm_db_app_password" "$sdir/mm_db_admin_password" \
                "$sdir/synapse_registration_shared_secret" "$sdir/signup_ip_hash_pepper"
  chmod 640 "$sdir/mm_db_app_password" "$sdir/mm_db_admin_password" \
            "$sdir/synapse_registration_shared_secret" "$sdir/signup_ip_hash_pepper"
}

# generate_secrets -- append-only: fills in every missing secret, never replaces one.
# Each step returns on failure by itself because a caller that checks the status
# (`generate_secrets || die …`) turns set -e off inside the function.
generate_secrets() {
  require_cmd openssl
  gen_literal LK_API_KEY "mmkey" || return 1
  gen_secret  LK_API_SECRET 64 || return 1
  gen_secret  MM_AS_TOKEN 64 || return 1
  gen_secret  MM_HS_TOKEN 64 || return 1
  gen_secret  MM_ADMIN_TOKEN 64 || return 1
  gen_secret  MM_JWT_SIGNING_KEY 64 || return 1
  gen_secret  MM_SWITCH_AUTH_SECRET 64 || return 1
  gen_secret  MM_SIGNUP_IP_HASH_PEPPER 64 || return 1
  # Encrypts secret settings saved from the dashboard (AES-256-GCM). Generated once;
  # losing it only means re-entering those secrets. Rotate with `mmctl rotate`.
  gen_secret  MM_SETTINGS_ENCRYPTION_KEY 64 || return 1
  gen_secret  SYNAPSE_REGISTRATION_SECRET 64 || return 1
  gen_secret  SYNAPSE_MACAROON_SECRET 64 || return 1
  gen_secret  SYNAPSE_FORM_SECRET 64 || return 1
  gen_secret  POSTGRES_SYNAPSE_PASS 32 || return 1
  gen_secret  POSTGRES_APP_ADMIN_PASS 32 || return 1
  gen_secret  POSTGRES_APP_PASS 32 || return 1
  # Password of the mm_fleet_runner role (deploy/sql/mm_fleet_runner_role.sql) that the
  # mm-fleet-runner service logs in with. Only compose interpolation reads it.
  gen_secret  POSTGRES_FLEET_RUNNER_PASS 32 || return 1
  # Password of the mm_media_worker role (deploy/sql/mm_media_worker_role.sql) that the
  # mm-media-worker service logs in with. Only compose interpolation reads it.
  gen_secret  POSTGRES_MEDIA_WORKER_PASS 32 || return 1
  gen_secret  REDIS_PASSWORD 32 || return 1
  gen_literal TURN_USER "mm" || return 1
  gen_secret  TURN_PASS 32 || return 1
  # MM_SYNAPSE_ADMIN_TOKEN: runtime-provisioned admin token for the Synapse
  # mmbot user.  Generated here as a placeholder; deploy/up.sh overwrites it
  # after Synapse registers the mmbot user on first boot.
  gen_secret  MM_SYNAPSE_ADMIN_TOKEN 64 || return 1
}
