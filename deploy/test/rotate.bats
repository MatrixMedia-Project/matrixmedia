load helper
# mmctl rotate — dry-run + dependency-map tests. No docker daemon needed:
# --dry-run / --list are pure shell over the static map in lib/rotate.sh.
setup() {
  setup_tmp
  source "$DEPLOY_ROOT/lib/common.sh"
  source "$DEPLOY_ROOT/lib/secrets.sh"
  source "$DEPLOY_ROOT/lib/rotate.sh"
  generate_secrets
}
teardown() { teardown_tmp; }

@test "rotate with no secret prints usage and exits non-zero" {
  run bash "$DEPLOY_ROOT/mmctl" rotate
  [ "$status" -ne 0 ]
  [[ "$output" == *"usage:"* ]]
}

@test "rotate --list names every rotatable secret and the paired ones" {
  run bash "$DEPLOY_ROOT/mmctl" rotate --list
  [ "$status" -eq 0 ]
  for k in MM_SWITCH_AUTH_SECRET MM_JWT_SIGNING_KEY POSTGRES_APP_ADMIN_PASS \
           LK_API_SECRET SYNAPSE_REGISTRATION_SECRET MM_SYNAPSE_ADMIN_TOKEN; do
    [[ "$output" == *"$k"* ]] || { echo "missing $k"; return 1; }
  done
  [[ "$output" == *"LK_API_KEY"* ]]
}

# Once mm-core has stored a Stripe or LNbits key, the database value wins and a new
# value in .env is ignored, so the hint must send the operator to the dashboard.
@test "rotate --list sends Stripe and LNbits key changes to the dashboard, with .env only as the fallback" {
  run bash "$DEPLOY_ROOT/mmctl" rotate --list
  [ "$status" -eq 0 ]
  for k in MM_STRIPE_SECRET_KEY MM_STRIPE_WEBHOOK_SECRET MM_LNBITS_INVOICE_KEY MM_LNBITS_ADMIN_KEY; do
    [[ "$output" == *"$k"* ]] || { echo "missing $k"; return 1; }
  done
  [[ "$output" == *"Operator Console -> Settings"*"Replace"*"Save"*"Apply & restart"* ]] || false
  [[ "$output" == *"ignored"* ]] || false
  [[ "$output" == *"Only without MM_SETTINGS_ENCRYPTION_KEY"*".env"* ]] || false
}

@test "rotate MM_SWITCH_AUTH_SECRET --dry-run prints the dual-recreate plan and mutates nothing" {
  before="$(cat "$MM_ROOT/.env.secrets")"
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_SWITCH_AUTH_SECRET --dry-run
  [ "$status" -eq 0 ]
  # verifier before signer, single force-recreate invocation
  [[ "$output" == *"--force-recreate mm-switch mm-core"* ]]
  [[ "$output" == *"_upsert_secret MM_SWITCH_AUTH_SECRET"* ]]
  # the secret value itself is never printed
  val="$(grep '^MM_SWITCH_AUTH_SECRET=' <<<"$before" | cut -d= -f2-)"
  [[ "$output" != *"$val"* ]]
  # no write
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
}

@test "rotate MM_JWT_SIGNING_KEY --dry-run flags the forced re-auth and targets mm-core only" {
  before="$(cat "$MM_ROOT/.env.secrets")"
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_JWT_SIGNING_KEY --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"FORCED RE-AUTH"* ]]
  [[ "$output" == *"--force-recreate mm-core"* ]]
  [[ "$output" != *"mm-switch"* ]]
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
}

@test "rotate POSTGRES_APP_ADMIN_PASS --dry-run includes the ALTER ROLE choreography" {
  before="$(cat "$MM_ROOT/.env.secrets")"
  run bash "$DEPLOY_ROOT/mmctl" rotate POSTGRES_APP_ADMIN_PASS --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"ALTER ROLE mm_admin"* ]]
  [[ "$output" == *"write_secret_files"* ]]      # one of the 4 file-backed secrets
  [[ "$output" == *"--force-recreate mm-core"* ]]
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
}

@test "rotate refuses unknown secrets" {
  run bash "$DEPLOY_ROOT/mmctl" rotate NOT_A_SECRET --dry-run
  [ "$status" -ne 0 ]
  [[ "$output" == *"unknown"* ]]
}

@test "rotate refuses paired literals with a pointer to the paired secret" {
  run bash "$DEPLOY_ROOT/mmctl" rotate LK_API_KEY --dry-run
  [ "$status" -ne 0 ]
  [[ "$output" == *"LK_API_SECRET"* ]]
}

# Dependency-map sanity: every key generate_secrets writes is classified —
# rotatable (has a plan) or a paired literal. A new gen_secret line without
# a rotation story must fail here.
@test "dependency map classifies every generated secret" {
  for k in $(grep -oE '^[[:space:]]*gen_(secret|literal)[[:space:]]+[A-Z0-9_]+' "$DEPLOY_ROOT/lib/secrets.sh" | awk '{print $2}'); do
    _rotate_plan "$k" >/dev/null 2>&1 && continue
    _rotate_paired "$k" >/dev/null 2>&1 && continue
    echo "unclassified secret in dependency map: $k"; return 1
  done
}

# Dependency-map sanity: every service in a restart set exists in the compose
# template, so `up -d --force-recreate <svc>` can never hit a typo.
@test "dependency map restart sets reference real compose services" {
  for k in $(_rotate_keys); do
    restarts="$(_rotate_plan "$k" | cut -d'|' -f5)"
    [ "$restarts" = "-" ] && continue
    IFS=',' read -ra svcs <<<"$restarts"
    for s in "${svcs[@]}"; do
      grep -qE "^  ${s}:" "$DEPLOY_ROOT/docker-compose.tmpl.yml" \
        || { echo "restart set for $k names unknown service: $s"; return 1; }
    done
  done
}

# Dependency-map sanity: the files/render flags match reality — file-backed
# secrets are exactly the ones write_secret_files materialises, and render=yes
# keys actually appear in a template.
@test "dependency map files/render flags match secrets.sh and templates/" {
  # files=yes keys must be exactly the 4 write_secret_files materialises
  file_backed=""
  for k in $(_rotate_keys); do
    [ "$(_rotate_plan "$k" | cut -d'|' -f2)" = yes ] && file_backed="$file_backed $k"
  done
  for k in POSTGRES_APP_PASS POSTGRES_APP_ADMIN_PASS SYNAPSE_REGISTRATION_SECRET MM_SIGNUP_IP_HASH_PEPPER; do
    [[ "$file_backed" == *"$k"* ]] || { echo "missing files=yes for $k"; return 1; }
  done
  [ "$(wc -w <<<"$file_backed")" -eq 4 ]
  # render=yes iff the key appears in some template
  for k in $(_rotate_keys); do
    render="$(_rotate_plan "$k" | cut -d'|' -f3)"
    in_tmpl=no
    grep -rqF -- "\${$k}" "$DEPLOY_ROOT/templates/" && in_tmpl=yes
    [ "$render" = "$in_tmpl" ] || { echo "$k render=$render but templates say $in_tmpl"; return 1; }
  done
}

@test "rotate dry-run never reads .env.secrets (works without any secrets file)" {
  rm -f "$MM_ROOT/.env.secrets"
  run bash "$DEPLOY_ROOT/mmctl" rotate REDIS_PASSWORD --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"lk-redis livekit livekit-egress livekit-ingress"* ]]
}

@test "rotate MM_SETTINGS_ENCRYPTION_KEY --dry-run shows the two-step re-encryption and mutates nothing" {
  before="$(cat "$MM_ROOT/.env.secrets")"
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_SETTINGS_ENCRYPTION_KEY --dry-run
  [ "$status" -eq 0 ]
  # `|| false`: under bash < 4.1 (macOS /bin/bash 3.2) a failing [[ ]] that is not the
  # last command does not fail the test.
  [[ "$output" == *"MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS"* ]] || false
  [[ "$output" == *"rows_on_previous_key"* ]] || false
  [[ "$output" == *"safe_mode = false"* ]] || false
  [[ "$output" == *"--force-recreate mm-core"* ]] || false
  [[ "$output" == *"resumes"* ]] || false
  [[ "$output" == *".env.secrets.after-generate"* ]] || false
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
}

@test "_rotate_stash_previous keeps the current key as _PREVIOUS" {
  cur="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  _rotate_stash_previous MM_SETTINGS_ENCRYPTION_KEY
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = "$cur" ]
  [ "$(file_mode "$MM_ROOT/.env.secrets")" = "600" ]
}

# A set _PREVIOUS may be the only key some stored secrets are still encrypted under.
@test "_rotate_stash_previous refuses to overwrite a _PREVIOUS that is already set" {
  _upsert_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS still-in-use-by-some-rows
  run _rotate_stash_previous MM_SETTINGS_ENCRYPTION_KEY
  [ "$status" -ne 0 ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = still-in-use-by-some-rows ]
}

@test "_rotate_settings_wait_reencrypted passes only when nothing is left on the previous key" {
  export MM_ROTATE_VERIFY_TRIES=2 MM_ROTATE_VERIFY_SLEEP=0
  _rotate_dc() { cat >/dev/null; echo '{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'; }
  run _rotate_settings_wait_reencrypted
  [ "$status" -eq 0 ]
  _rotate_dc() { cat >/dev/null; echo '{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":2,"secret_problems":[]}'; }
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
}

# mm-core also reports rows_on_previous_key as 0 when it has NO settings key loaded at
# all, so rows==0 alone is not proof of re-encryption. The wait must also see
# encryption_key_configured:true, or a mm-core that failed to load any key looks
# indistinguishable from "done" and rotate.sh would delete the only key able to
# decrypt the stored secrets.
@test "_rotate_settings_wait_reencrypted requires encryption_key_configured true, not just rows==0" {
  export MM_ROTATE_VERIFY_TRIES=1 MM_ROTATE_VERIFY_SLEEP=0
  _rotate_dc() { cat >/dev/null; echo '{"safe_mode":false,"encryption_key_configured":false,"rows_on_previous_key":0,"secret_problems":[]}'; }
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
  _rotate_dc() { cat >/dev/null; echo '{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'; }
  run _rotate_settings_wait_reencrypted
  [ "$status" -eq 0 ]
}

# With MM_SETTINGS_SAFE_MODE set, mm-core skips the stored settings, so a stored secret
# it cannot decrypt never reaches secret_problems; in automatic safe mode the stored
# values are not in use either. Only a mm-core that runs its stored settings can
# confirm the re-encryption, so the wait also needs "safe_mode":false.
@test "_rotate_settings_wait_reencrypted requires safe_mode false" {
  export MM_ROTATE_VERIFY_TRIES=1 MM_ROTATE_VERIFY_SLEEP=0
  done_flags='"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'
  _rotate_dc() { cat >/dev/null; printf '%s' "$BODY"; }
  BODY='{"safe_mode":true,"break_glass":true,'"$done_flags"
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
  BODY='{"safe_mode":true,"break_glass":false,'"$done_flags"
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
  BODY='{'"$done_flags"                      # no safe_mode field at all
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
  BODY='{"safe_mode":false,"break_glass":false,'"$done_flags"
  run _rotate_settings_wait_reencrypted
  [ "$status" -eq 0 ]
}

@test "_rotate_settings_wait_cause names safe mode, and which kind, without quoting the body" {
  done_flags='"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'
  run _rotate_settings_wait_cause '{"safe_mode":true,"break_glass":true,"safe_mode_reason":"reason-marker-7f3a",'"$done_flags" MM_SETTINGS_ENCRYPTION_KEY
  [[ "$output" == *"MM_SETTINGS_SAFE_MODE is set"* ]] || false
  [[ "$output" != *"reason-marker-7f3a"* ]] || false
  run _rotate_settings_wait_cause '{"safe_mode":true,"break_glass":false,"safe_mode_reason":"reason-marker-7f3a",'"$done_flags" MM_SETTINGS_ENCRYPTION_KEY
  [[ "$output" == *"safe mode"* ]] || false
  [[ "$output" != *"MM_SETTINGS_SAFE_MODE is set"* ]] || false
  [[ "$output" != *"reason-marker-7f3a"* ]] || false
}

# The real GET /_mm/admin/v1/settings body carries the full setting schema and every
# value before the two flags, far past a pipe buffer. The wait must read any such body.
# (`printf | grep -q` under pipefail happens to read a one-line body to its end, but on
# a body with line breaks grep -q exits at the first matching line, printf dies of
# SIGPIPE, and pipefail turns the match into a miss.)
@test "_rotate_settings_wait_reencrypted reads a settings body larger than 128 KiB" {
  export MM_ROTATE_VERIFY_TRIES=1 MM_ROTATE_VERIFY_SLEEP=0
  pad="$(head -c 140000 /dev/zero | tr '\0' 'x')"
  head='{"schema":"'"$pad"'","values":{},"safe_mode":false,"safe_mode_reason":null,"loaded_rev":1,"current_rev":1,"pending_restart":[],'
  tail=',"secret_problems":[],"live_reload_error":null,"demo":false}'
  [ "${#head}" -gt 131072 ]
  _rotate_dc() { cat >/dev/null; printf '%s' "$BIG_BODY"; }
  # field order as mm-core serialises it: schema ... flags ... secret_problems
  BIG_BODY="$head"'"encryption_key_configured":true,"rows_on_previous_key":0'"$tail"
  run _rotate_settings_wait_reencrypted
  [ "$status" -eq 0 ]
  BIG_BODY="$head"'"encryption_key_configured":false,"rows_on_previous_key":0'"$tail"
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
  # a body with line breaks, flags before the schema
  BIG_BODY="$(printf '{"safe_mode":false,"encryption_key_configured":true,\n"rows_on_previous_key":0,\n"secret_problems":[],\n"schema":"%s"\n}' "$pad")"
  run _rotate_settings_wait_reencrypted
  [ "$status" -eq 0 ]
  # ":0" must not match the start of a longer number
  BIG_BODY="$head"'"encryption_key_configured":true,"rows_on_previous_key":05'"$tail"
  run _rotate_settings_wait_reencrypted
  [ "$status" -ne 0 ]
}

@test "the admin token reaches curl on stdin, never in argv" {
  export MM_ROTATE_VERIFY_TRIES=1 MM_ROTATE_VERIFY_SLEEP=0
  tok="$(read_secret MM_ADMIN_TOKEN)"
  _rotate_dc() { echo "ARGV: $*" >> "$MM_ROOT/argv"; cat >> "$MM_ROOT/stdin"; echo '{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'; }
  _rotate_settings_wait_reencrypted
  [ -f "$MM_ROOT/argv" ]                       # the stub really ran
  run grep -q "$tok" "$MM_ROOT/argv"
  [ "$status" -eq 1 ]                          # 1 = no match (2 would be a read error)
  grep -q "Bearer $tok" "$MM_ROOT/stdin"
}

# ── rotate_secret, end to end, for the settings key ─────────────────────────
# No docker and no network: a `docker` on PATH that fails if anything reaches it,
# _rotate_dc recording every compose call and answering the settings API with
# $SETTINGS_BODY, and wait_healthy stubbed to succeed.
_rotation_harness() {
  mkdir -p "$MM_ROOT/bin"
  printf '#!/bin/sh\necho "real docker must not run in tests" >&2\nexit 99\n' > "$MM_ROOT/bin/docker"
  chmod +x "$MM_ROOT/bin/docker"
  PATH="$MM_ROOT/bin:$PATH"
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  printf 'services:\n  mm-core:\n    environment:\n      MM_SETTINGS_ENCRYPTION_KEY: ${MM_SETTINGS_ENCRYPTION_KEY:-}\n      MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS: ${MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS:-}\n' \
    > "$MM_ROOT/docker-compose.yml"
  export MM_ROTATE_VERIFY_TRIES=2 MM_ROTATE_VERIFY_SLEEP=0
  SETTINGS_BODY='{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'
  _rotate_dc() {
    echo "$*" >> "$MM_ROOT/dc-calls"
    if [ "$1" = exec ]; then cat >/dev/null; printf '%s' "$SETTINGS_BODY"; fi
    return 0
  }
  wait_healthy() { return 0; }
}

@test "rotate_secret resumes an unfinished settings-key rotation instead of generating another key" {
  _rotation_harness
  # State left by a run that stopped after phase 1: rows may still be on k0.
  k0="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  k1="$(openssl rand -hex 32)"
  _upsert_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS "$k0"
  _upsert_secret MM_SETTINGS_ENCRYPTION_KEY "$k1"
  _rotate_settings_wait_reencrypted() {
    read_secret MM_SETTINGS_ENCRYPTION_KEY > "$MM_ROOT/key-at-wait"
    read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS > "$MM_ROOT/previous-at-wait"
    return 0
  }
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -eq 0 ]
  [[ "$output" == *"resuming an unfinished rotation of MM_SETTINGS_ENCRYPTION_KEY"* ]] || false
  [[ "$output" == *"finished an earlier rotation of MM_SETTINGS_ENCRYPTION_KEY; no new key was generated"* ]] || false
  [[ "$output" != *"rotated MM_SETTINGS_ENCRYPTION_KEY."* ]] || false
  # up to the wait, neither key moved
  [ "$(cat "$MM_ROOT/key-at-wait")" = "$k1" ]
  [ "$(cat "$MM_ROOT/previous-at-wait")" = "$k0" ]
  # after the confirmed wait: k1 stays, _PREVIOUS is gone
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY)" = "$k1" ]
  [ -z "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" ]
  # phases 0-1 skipped (no backup); mm-core recreated for phase 3 and again after the drop
  [ ! -e "$MM_ROOT/rotate-backups" ]
  [ "$(grep -c '^up -d --force-recreate mm-core$' "$MM_ROOT/dc-calls")" -eq 2 ]
}

@test "rotate_secret keeps a 0600 copy of .env.secrets holding the NEW key beside the phase-0 backup" {
  _rotation_harness
  old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -eq 0 ]
  new="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  [ -n "$new" ]
  [ "$new" != "$old" ]
  after="$(echo "$MM_ROOT"/rotate-backups/*/.env.secrets.after-generate)"
  [ -f "$after" ]
  [ "$(file_mode "$after")" = "600" ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY "$after")" = "$new" ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS "$after")" = "$old" ]
  # the phase-0 backup beside it holds only the old key
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY "$(dirname "$after")/.env.secrets")" = "$old" ]
  # the finished rotation dropped _PREVIOUS
  [ -z "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" ]
  [[ "$output" != *"$old"* ]] || false
  [[ "$output" != *"$new"* ]] || false
}

@test "both unhealthy-stack exits of a settings-key rotation say NOT to restore the phase-0 .env.secrets" {
  _rotation_harness
  wait_healthy() {
    local n; n="$(cat "$MM_ROOT/healthy-calls" 2>/dev/null || echo 0)"; n=$((n + 1))
    echo "$n" > "$MM_ROOT/healthy-calls"
    [ "$n" -ne "$UNHEALTHY_AT" ]
  }
  # 1 = unhealthy after the phase-3 recreate; 2 = after the recreate that follows the drop
  for UNHEALTHY_AT in 1 2; do
    rm -f "$MM_ROOT/healthy-calls"
    _remove_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS
    old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
    run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
    [ "$status" -ne 0 ]
    new="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
    [ "$new" != "$old" ]
    [[ "$output" == *"Do NOT restore"* ]] || false
    [[ "$output" == *"/rotate-backups/"*"/.env.secrets.after-generate"* ]] || false
    [[ "$output" == *"MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS to the current value"* ]] || false
    [[ "$output" == *"re-encrypt"* ]] || false
    [[ "$output" != *"$old"* ]] || false
    [[ "$output" != *"$new"* ]] || false
  done
}

@test "rotate_secret stops, keeping both keys, when mm-core reports no settings key loaded" {
  _rotation_harness
  old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  # rows_on_previous_key is 0 here only because no key is loaded at all.
  SETTINGS_BODY='{"safe_mode":false,"encryption_key_configured":false,"rows_on_previous_key":0,"secret_problems":[]}'
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -ne 0 ]
  [[ "$output" == *"did not load the new"* ]] || false
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = "$old" ]
  new="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  [[ "$new" =~ ^[0-9a-f]{64}$ ]] || false
  [ "$new" != "$old" ]
  # recreated for phase 3 only; the drop and its recreate never happened
  [ "$(grep -c '^up -d --force-recreate mm-core$' "$MM_ROOT/dc-calls")" -eq 1 ]
}

@test "a failed re-encryption wait names its cause and never prints a key or the admin token" {
  _rotation_harness
  tok="$(read_secret MM_ADMIN_TOKEN)"
  SETTINGS_BODY=''
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -ne 0 ]
  [[ "$output" == *"unreachable, or the admin token was rejected"* ]] || false
  unreachable_output="$output"
  SETTINGS_BODY='{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":3,"secret_problems":[]}'
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1      # resumes: _PREVIOUS is set now
  [ "$status" -ne 0 ]
  [[ "$output" == *"re-encryption not finished: 3 stored secret(s) still on the previous key"* ]] || false
  key="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  prev="$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)"
  [ -n "$key" ]
  [ -n "$prev" ]
  for out in "$unreachable_output" "$output"; do
    [[ "$out" != *"$key"* ]] || false
    [[ "$out" != *"$prev"* ]] || false
    [[ "$out" != *"$tok"* ]] || false
  done
}

@test "rotate_secret refuses the settings key while docker-compose.yml does not pass it to mm-core" {
  _rotation_harness
  printf 'services:\n  mm-core:\n    environment:\n      MM_ADMIN_TOKEN: x\n' > "$MM_ROOT/docker-compose.yml"
  before="$(cat "$MM_ROOT/.env.secrets")"
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -ne 0 ]
  [[ "$output" == *"does not pass MM_SETTINGS_ENCRYPTION_KEY to mm-core"* ]] || false
  [[ "$output" == *"re-run install.sh"* ]] || false
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
  [ ! -e "$MM_ROOT/rotate-backups" ]
  [ ! -e "$MM_ROOT/dc-calls" ]
}

# A compose file that passes the key but not _PREVIOUS (e.g. only the key line added by
# hand after the upgrade warning) boots mm-core with no previous key: rows on the old
# key read as 0 "on previous", the wait would pass, and dropping _PREVIOUS loses them.
@test "rotate_secret refuses the settings key while docker-compose.yml passes the key but not _PREVIOUS" {
  _rotation_harness
  printf 'services:\n  mm-core:\n    environment:\n      MM_SETTINGS_ENCRYPTION_KEY: ${MM_SETTINGS_ENCRYPTION_KEY:-}\n' \
    > "$MM_ROOT/docker-compose.yml"
  before="$(cat "$MM_ROOT/.env.secrets")"
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -ne 0 ]
  [[ "$output" == *"MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS"* ]] || false
  [[ "$output" == *"re-run install.sh"* ]] || false
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
  [ ! -e "$MM_ROOT/rotate-backups" ]
  [ ! -e "$MM_ROOT/dc-calls" ]
}

@test "rotate_secret keeps _PREVIOUS when mm-core reports secret problems" {
  _rotation_harness
  old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  # configured and nothing "on previous", yet a stored secret cannot be decrypted
  SETTINGS_BODY='{"safe_mode":false,"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[{"key":"stripe.secret_key","reason":"cannot decrypt"}]}'
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -ne 0 ]
  [[ "$output" == *"mm-core reports secret problems (see Settings in the Operator Console); MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS kept"* ]] || false
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = "$old" ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY)" != "$old" ]
  [ "$(grep -c '^up -d --force-recreate mm-core$' "$MM_ROOT/dc-calls")" -eq 1 ]
}

@test "rotate_secret keeps _PREVIOUS and names safe mode when mm-core runs in safe mode" {
  _rotation_harness
  old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  # Everything else reads as done; only safe mode stands in the way.
  SETTINGS_BODY='{"safe_mode":true,"break_glass":true,"safe_mode_reason":"reason-marker-7f3a","encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1
  [ "$status" -ne 0 ]
  [[ "$output" == *"MM_SETTINGS_SAFE_MODE is set"* ]] || false
  [[ "$output" == *"MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS left in place"* ]] || false
  [[ "$output" != *"reason-marker-7f3a"* ]] || false
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = "$old" ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY)" != "$old" ]
  [ "$(grep -c '^up -d --force-recreate mm-core$' "$MM_ROOT/dc-calls")" -eq 1 ]

  SETTINGS_BODY='{"safe_mode":true,"break_glass":false,"safe_mode_reason":"reason-marker-7f3a","encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}'
  run rotate_secret MM_SETTINGS_ENCRYPTION_KEY 0 1      # resumes: _PREVIOUS is still set
  [ "$status" -ne 0 ]
  [[ "$output" == *"safe mode"* ]] || false
  [[ "$output" != *"reason-marker-7f3a"* ]] || false
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = "$old" ]
}

# ── the compose calls a rotation makes ──────────────────────────────────────
# On an installed host the image pins (MM_REGISTRY, MM_VERSION) live only in
# versions.env; install.sh keeps them out of .env. A compose call that is not handed
# versions.env resolves the mm-core image to '/matrixmedia-mm-core:' and fails.

@test "_rotate_dc hands compose versions.env first, then .env and .env.secrets" {
  mkdir -p "$MM_ROOT/bin"
  # Stand-in docker: one line per argument.
  printf '#!/bin/sh\nfor a in "$@"; do printf "%%s\\n" "$a"; done > "$MM_ROOT/docker-argv"\n' > "$MM_ROOT/bin/docker"
  chmod +x "$MM_ROOT/bin/docker"
  PATH="$MM_ROOT/bin:$PATH"
  : > "$MM_ROOT/versions.env"; : > "$MM_ROOT/.env"

  _rotate_dc up -d --force-recreate mm-core
  expected="$(printf '%s\n' compose \
    --env-file "$MM_ROOT/versions.env" --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -p matrixmedia up -d --force-recreate mm-core)"
  [ "$(cat "$MM_ROOT/docker-argv")" = "$expected" ]

  # An install that predates versions.env still gets .env then .env.secrets.
  rm "$MM_ROOT/versions.env"
  _rotate_dc ps
  expected="$(printf '%s\n' compose \
    --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -p matrixmedia ps)"
  [ "$(cat "$MM_ROOT/docker-argv")" = "$expected" ]
}

# A host shaped like a fresh install: the shipped versions.env and compose template, an
# installer-style .env without image pins, generated secrets. mmctl runs for real (with
# its set -euo pipefail). `docker` is a stand-in that records every call, reports every
# container healthy, answers the settings API with $MM_ROOT/settings-body and, like
# compose, cannot resolve the mm-core image unless the env files it is handed define
# MM_REGISTRY and MM_VERSION. The up call numbered in $MM_ROOT/fail-up-at fails.
_stock_host() {
  cp "$DEPLOY_ROOT/versions.env" "$MM_ROOT/versions.env"
  cp "$DEPLOY_ROOT/docker-compose.tmpl.yml" "$MM_ROOT/docker-compose.yml"
  printf 'MM_DOMAIN=example.com\nMM_DEMO_MODE=false\n' > "$MM_ROOT/.env"
  printf '%s' '{"safe_mode":false,"break_glass":false,"encryption_key_configured":true,"rows_on_previous_key":0,"secret_problems":[]}' \
    > "$MM_ROOT/settings-body"
  mkdir -p "$MM_ROOT/bin"
  cat > "$MM_ROOT/bin/docker" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$MM_ROOT/docker-calls"
if [ "$1" = ps ]; then echo "Up 3 seconds (healthy)"; exit 0; fi
[ "$1" = compose ] || exit 0
shift
envs=""
while [ $# -gt 0 ]; do
  case "$1" in
    --env-file) envs="$envs $2"; shift 2 ;;
    -f|-p) shift 2 ;;
    *) break ;;
  esac
done
case "${1:-}" in
  exec)
    cat >/dev/null
    cat "$MM_ROOT/settings-body" ;;
  up)
    n=$(( $(cat "$MM_ROOT/up-count" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$MM_ROOT/up-count"
    if [ "$n" = "$(cat "$MM_ROOT/fail-up-at" 2>/dev/null)" ]; then
      echo "compose: simulated failure of up call $n" >&2; exit 1
    fi
    reg=""; ver=""
    for e in $envs; do
      v="$(grep '^MM_REGISTRY=' "$e" | tail -n 1 | cut -d= -f2-)"; [ -n "$v" ] && reg="$v"
      v="$(grep '^MM_VERSION=' "$e" | tail -n 1 | cut -d= -f2-)"; [ -n "$v" ] && ver="$v"
    done
    if [ -z "$reg" ] || [ -z "$ver" ]; then
      echo "unable to get image '$reg/matrixmedia-mm-core:$ver': invalid reference format" >&2; exit 1
    fi ;;
esac
exit 0
SH
  chmod +x "$MM_ROOT/bin/docker"
  PATH="$MM_ROOT/bin:$PATH"
  export MM_ROTATE_VERIFY_TRIES=2 MM_ROTATE_VERIFY_SLEEP=0
}

@test "mmctl rotate MM_SETTINGS_ENCRYPTION_KEY finishes on a stock install, every compose call pinned by versions.env" {
  _stock_host
  old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  tok="$(read_secret MM_ADMIN_TOKEN)"
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_SETTINGS_ENCRYPTION_KEY --yes
  out="$output"
  [ "$status" -eq 0 ]
  new="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  [[ "$new" =~ ^[0-9a-f]{64}$ ]] || false
  [ "$new" != "$old" ]
  [ -z "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" ]
  [ "$(cat "$MM_ROOT/up-count")" -eq 2 ]          # the phase-3 recreate and the one after the drop
  pinned="compose --env-file $MM_ROOT/versions.env --env-file $MM_ROOT/.env --env-file $MM_ROOT/.env.secrets -f $MM_ROOT/docker-compose.yml -p matrixmedia "
  n=0
  while IFS= read -r call; do
    case "$call" in "ps "*) continue ;; esac
    [[ "$call" == "$pinned"* ]] || { echo "compose call without the pinned env files: $call"; return 1; }
    n=$((n + 1))
  done < "$MM_ROOT/docker-calls"
  [ "$n" -ge 3 ]                                  # up, the settings API (exec), up
  # neither key nor the admin token reached argv or the output
  for v in "$old" "$new" "$tok"; do
    run grep -qF "$v" "$MM_ROOT/docker-calls"
    [ "$status" -eq 1 ]
    [[ "$out" != *"$v"* ]] || false
  done
}

@test "a failing compose up in a settings-key rotation gives the key-specific way back and keeps _PREVIOUS" {
  _stock_host
  old="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  echo 1 > "$MM_ROOT/fail-up-at"                  # the phase-3 recreate fails
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_SETTINGS_ENCRYPTION_KEY --yes
  [ "$status" -ne 0 ]
  [[ "$output" == *"docker compose could not recreate mm-core"* ]] || false
  [[ "$output" == *"Do NOT restore"* ]] || false
  [[ "$output" == *"/rotate-backups/"*"/.env.secrets.after-generate"* ]] || false
  [[ "$output" == *"MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS to the current value"* ]] || false
  new="$(read_secret MM_SETTINGS_ENCRYPTION_KEY)"
  [ "$new" != "$old" ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS)" = "$old" ]
  [ "$(cat "$MM_ROOT/up-count")" -eq 1 ]          # stopped there: no settings wait, no drop
  run grep -q ' exec ' "$MM_ROOT/docker-calls"
  [ "$status" -eq 1 ]

  # Re-run: it resumes, and this time the recreate after the drop fails.
  rm -f "$MM_ROOT/up-count"
  echo 2 > "$MM_ROOT/fail-up-at"
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_SETTINGS_ENCRYPTION_KEY --yes
  [ "$status" -ne 0 ]
  [[ "$output" == *"resuming an unfinished rotation"* ]] || false
  [[ "$output" == *"Do NOT restore"* ]] || false
  [ "$(cat "$MM_ROOT/up-count")" -eq 2 ]
  [ "$(read_secret MM_SETTINGS_ENCRYPTION_KEY)" = "$new" ]
  [[ "$output" != *"$old"* ]] || false
  [[ "$output" != *"$new"* ]] || false
}

@test "a failing compose up in any other rotation points to the rollback section" {
  _stock_host
  echo 1 > "$MM_ROOT/fail-up-at"
  run bash "$DEPLOY_ROOT/mmctl" rotate MM_ADMIN_TOKEN --yes
  [ "$status" -ne 0 ]
  [[ "$output" == *"docker compose could not recreate mm-core"*"see the rollback section of deploy/docs/rotation-runbooks.md"* ]] || false
  [ "$(cat "$MM_ROOT/up-count")" -eq 1 ]
}
