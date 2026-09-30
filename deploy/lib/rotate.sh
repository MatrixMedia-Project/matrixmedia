# shellcheck shell=bash
# Secret rotation for the compose path (mmctl rotate).
# Requires: common.sh, secrets.sh, render.sh, up.sh, bootstrap.sh sourced;
# $MM_ROOT set (defaults /opt/mm).
#
# Hard rules:
#   - secret values never appear in argv, logs, or xtrace output;
#   - gen_secret is NEVER called (its append-if-absent contract is untouched);
#     all writes go through _upsert_secret (secrets.sh);
#   - restarts always use `up -d --force-recreate` — `docker compose restart`
#     does NOT re-read env files and would leave consumers on the old value.
: "${MM_ROOT:=/opt/mm}"

# Compose wrapper with the same env-file chain as every other compose call
# (compose_env_files in common.sh: versions.env first when present, then .env and
# .env.secrets; it also sets COMPOSE_PROFILES) and the project name. versions.env
# alone defines MM_REGISTRY/MM_VERSION on an installed host: without it the mm-core
# image reference interpolates to '/matrixmedia-mm-core:' and every recreate fails.
_rotate_dc() {
  compose_env_files
  docker compose "${MM_ENV_FILES[@]}" -f "$MM_ROOT/docker-compose.yml" -p matrixmedia "$@"
}

# ── Dependency map ──────────────────────────────────────────────────────────
# _rotate_plan KEY -> echoes "hexlen|files|render|alter|restart_csv|note"
#   hexlen   length of the new `openssl rand -hex` value (0 = not generated here)
#   files    yes -> write_secret_files (KEY is one of the 4 file-backed secrets)
#   render   yes -> render_templates   (KEY appears in templates/*.tmpl.*)
#   alter    external-store step: none | alter_synapse | alter_mm_admin |
#            alter_mm_app | capture_admin
#   restarts services to `up -d --force-recreate`, IN ORDER, single invocation
#            (verifier before signer; store before client); "-" = none
#   note     expected impact window (must not contain "|")
# Every consumer below is traced through docker-compose.tmpl.yml + templates/.
_rotate_plan() {
  case "$1" in
    LK_API_SECRET)               echo "64|no|yes|none|livekit,livekit-egress,livekit-ingress,mm-core,lk-jwt-service|in-room media drops on livekit restart; most disruptive rotation - schedule it" ;;
    MM_AS_TOKEN)                 echo "64|no|yes|none|synapse,mm-core|appservice txns 403+queue during the window and retry; rotate together with MM_HS_TOKEN" ;;
    MM_HS_TOKEN)                 echo "64|no|yes|none|synapse,mm-core|appservice txns 403+queue during the window and retry; rotate together with MM_AS_TOKEN" ;;
    MM_ADMIN_TOKEN)              echo "64|no|no|none|mm-core|one mm-core recreate; healthcheck label re-interpolates on recreate" ;;
    MM_JWT_SIGNING_KEY)          echo "64|no|no|none|mm-core|FORCED RE-AUTH: every MM session/refresh/admin JWT is invalidated; Matrix access tokens unaffected" ;;
    MM_SWITCH_AUTH_SECRET)       echo "64|no|no|none|mm-switch,mm-core|single dual-recreate; ~5-10s control-plane blip; live WebRTC sessions unaffected" ;;
    MM_SIGNUP_IP_HASH_PEPPER)    echo "64|yes|no|none|mm-core|one mm-core recreate; no user-visible effect" ;;
    MM_SETTINGS_ENCRYPTION_KEY)  echo "64|no|no|reencrypt_settings|mm-core|two mm-core recreates (~10s API gap each); secrets re-encrypted at startup; nothing user-visible" ;;
    SYNAPSE_REGISTRATION_SECRET) echo "64|yes|yes|none|synapse,mm-core|signup endpoint errors for one synapse+mm-core recreate" ;;
    SYNAPSE_MACAROON_SECRET)     echo "64|no|yes|none|synapse|compromise-only rotation; some login flows re-auth" ;;
    SYNAPSE_FORM_SECRET)         echo "64|no|yes|none|synapse|one synapse restart; low impact" ;;
    POSTGRES_SYNAPSE_PASS)       echo "32|no|yes|alter_synapse|synapse|ALTER ROLE synapse then recreate synapse (~30s; clients retry, federation queues)" ;;
    POSTGRES_APP_ADMIN_PASS)     echo "32|yes|yes|alter_mm_admin|mm-core|ALTER ROLE mm_admin then recreate mm-core (~10-30s API gap; DB uninterrupted)" ;;
    POSTGRES_APP_PASS)           echo "32|yes|yes|alter_mm_app|-|no live consumer in the compose file; ALTER + secret-file refresh only" ;;
    REDIS_PASSWORD)              echo "32|no|yes|none|lk-redis,livekit,livekit-egress,livekit-ingress|LiveKit room-state blip; active calls drop" ;;
    TURN_PASS)                   echo "32|no|no|none|coturn,mm-switch|established relays drop and ICE-restart" ;;
    MM_SYNAPSE_ADMIN_TOKEN)      echo "0|no|no|capture_admin|mm-core|re-login as the server owner (prompts for credentials); not randomly generated" ;;
    *) return 1 ;;
  esac
}

# Literals that only make sense rotated together with their paired secret.
# _rotate_paired KEY -> echoes the partner; non-zero if KEY is not a literal.
_rotate_paired() {
  case "$1" in
    LK_API_KEY)      echo "LK_API_SECRET" ;;
    TURN_USER)       echo "TURN_PASS" ;;
    *) return 1 ;;
  esac
}

# Rotatable keys, in inventory order (drives --list and the map-sanity test).
_rotate_keys() {
  echo "LK_API_SECRET MM_AS_TOKEN MM_HS_TOKEN MM_ADMIN_TOKEN MM_JWT_SIGNING_KEY \
MM_SWITCH_AUTH_SECRET MM_SIGNUP_IP_HASH_PEPPER MM_SETTINGS_ENCRYPTION_KEY \
SYNAPSE_REGISTRATION_SECRET \
SYNAPSE_MACAROON_SECRET SYNAPSE_FORM_SECRET POSTGRES_SYNAPSE_PASS \
POSTGRES_APP_ADMIN_PASS POSTGRES_APP_PASS REDIS_PASSWORD \
TURN_PASS MM_SYNAPSE_ADMIN_TOKEN"
}

rotate_list() {
  local k plan len files render alter restarts note
  printf '%-28s %-55s %s\n' "SECRET" "RECREATE (in order)" "EXPECTED WINDOW"
  for k in $(_rotate_keys); do
    plan="$(_rotate_plan "$k")"
    IFS='|' read -r len files render alter restarts note <<<"$plan"
    printf '%-28s %-55s %s\n' "$k" "$restarts" "$note"
  done
  echo
  echo "Paired literals (rotate via their partner): LK_API_KEY -> LK_API_SECRET," \
       "TURN_USER -> TURN_PASS"
  echo "Operator-supplied (MM_STRIPE_SECRET_KEY, MM_STRIPE_WEBHOOK_SECRET," \
       "MM_LNBITS_INVOICE_KEY, MM_LNBITS_ADMIN_KEY): rotate at the provider, then" \
       "Operator Console -> Settings -> Replace, Save, Apply & restart. Once mm-core" \
       "has stored a key, the database value wins and a new value in .env is ignored." \
       "Only without MM_SETTINGS_ENCRYPTION_KEY: paste it into $MM_ROOT/.env, then" \
       "recreate mm-core (mmctl start)."
  echo "Runbooks: deploy/docs/rotation-runbooks.md"
}

# _rotate_print_plan KEY PLAN -- the --dry-run output: ordered phases, no
# secret value is ever read or written here (pure shell on the static map).
_rotate_print_plan() {
  local key="$1" len files render alter restarts note
  IFS='|' read -r len files render alter restarts note <<<"$2"
  echo "rotation plan for $key (dry-run, nothing changed):"
  if [ "$alter" = reencrypt_settings ]; then
    echo "  resume              if ${key}_PREVIOUS is already set, an earlier rotation did not finish and this run resumes it: phases 0-1 are skipped (no new key, ${key}_PREVIOUS kept) and it continues at phase 3"
  fi
  echo "  phase 0  PRECHECK   backup .env.secrets + secrets/ + config/ -> $MM_ROOT/rotate-backups/<ts>/ (mode 700)"
  if [ "$alter" = capture_admin ]; then
    echo "  phase 1  GENERATE   none — re-login as the server owner captures a fresh Synapse token (capture_admin_token)"
  elif [ "$alter" = reencrypt_settings ]; then
    echo "  phase 1  GENERATE   copy the current value to ${key}_PREVIOUS, then openssl rand -hex $len -> _upsert_secret $key (values never logged)"
  else
    echo "  phase 1  GENERATE   openssl rand -hex $len -> _upsert_secret $key (value never logged)"
  fi
  echo "  phase 1b KEEP       copy the updated .env.secrets -> $MM_ROOT/rotate-backups/<ts>/.env.secrets.after-generate (mode 600; holds the NEW value)"
  if [ "$files" = yes ]; then
    echo "  phase 2  PROPAGATE  write_secret_files (refresh $MM_ROOT/secrets/, newline-free)"
  fi
  if [ "$render" = yes ]; then
    echo "  phase 2  PROPAGATE  render_templates ($key appears in templates/*.tmpl.*)"
  fi
  case "$alter" in
    alter_synapse)  echo "  phase 2  PROPAGATE  ALTER ROLE synapse  in the synapse postgres (psql via container socket, value on stdin)" ;;
    alter_mm_admin) echo "  phase 2  PROPAGATE  ALTER ROLE mm_admin in mm-postgres (psql via container socket, value on stdin)" ;;
    alter_mm_app)   echo "  phase 2  PROPAGATE  ALTER ROLE mm_app   in mm-postgres (psql via container socket, value on stdin)" ;;
  esac
  if [ "$restarts" = "-" ]; then
    echo "  phase 3  RESTART    none (no live consumer)"
  else
    echo "  phase 3  RESTART    docker compose up -d --force-recreate ${restarts//,/ }   (single invocation, in order; never 'restart' — it keeps stale env)"
  fi
  if [ "$alter" = reencrypt_settings ]; then
    echo "  phase 3b REENCRYPT  wait until GET /_mm/admin/v1/settings reports safe_mode = false, encryption_key_configured = true, rows_on_previous_key = 0 and no secret_problems, then drop ${key}_PREVIOUS and up -d --force-recreate mm-core again"
  fi
  echo "  phase 4  VERIFY     wait_healthy + mmctl doctor + the $key probes in deploy/docs/rotation-runbooks.md"
  echo "  phase 5  INVALIDATE negative probe with the old value; purge the backup after the soak window"
  echo "  expected window: $note"
}

# Directory of this run's phase-0 backup ("" until one is taken, and on a resumed run).
_ROTATE_BACKUP_DIR=""

_rotate_backup() {
  local ts dir
  ts="$(date +%Y%m%d-%H%M%S)"
  dir="$MM_ROOT/rotate-backups/$ts"
  mkdir -p "$dir"
  chmod 700 "$MM_ROOT/rotate-backups" "$dir"
  cp -p "$MM_ROOT/.env.secrets" "$dir/.env.secrets"
  [ -d "$MM_ROOT/secrets" ] && cp -pR "$MM_ROOT/secrets" "$dir/secrets"
  [ -d "$MM_ROOT/config" ]  && cp -pR "$MM_ROOT/config"  "$dir/config"
  chmod -R go-rwx "$dir"
  _ROTATE_BACKUP_DIR="$dir"
  log "rotate: backup at $dir (contains the OLD secret values — purge after the soak window)"
}

# _rotate_backup_after_generate -- right after phase 1, keep a second copy of
# .env.secrets, holding the NEW value, beside the phase-0 backup. Once mm-core has
# re-encrypted the stored settings under a new MM_SETTINGS_ENCRYPTION_KEY, the
# phase-0 copy no longer holds a key that can decrypt them; this copy does.
_rotate_backup_after_generate() {
  local dst="$_ROTATE_BACKUP_DIR/.env.secrets.after-generate"
  cp "$MM_ROOT/.env.secrets" "$dst"
  chmod 600 "$dst"
  log "rotate: copy with the NEW value at $dst (purge it with the rest of the backup after the soak window)"
}

# _alter_role_password SERVICE SUPERUSER DB ROLE -- new password on stdin
# (single line). Runs psql inside the Postgres container over the local
# socket (trust/peer — never blocked by the credential being rotated).
# The value reaches psql via stdin-built SQL, never argv, never logs.
_alter_role_password() {
  local svc="$1" su="$2" db="$3" role="$4" new
  IFS= read -r new
  printf "ALTER ROLE %s PASSWORD '%s';\n" "$role" "$new" \
    | _rotate_dc exec -T "$svc" psql -q -v ON_ERROR_STOP=1 -U "$su" -d "$db" -f - \
    || die "rotate: ALTER ROLE $role failed — DB still holds the OLD password (env already updated; restore from rotate-backups or re-run)"
  log "rotate: ALTER ROLE $role applied in $svc"
}

# MM_SYNAPSE_ADMIN_TOKEN is not generated — it IS a Synapse access token for
# the owner account; "rotation" = log in again and persist the fresh token.
_rotate_capture_admin() {
  local domain user pass
  domain="$(grep '^MM_DOMAIN=' "$MM_ROOT/.env" | head -1 | cut -d= -f2-)"
  [ -n "$domain" ] || die "rotate: MM_DOMAIN not found in $MM_ROOT/.env"
  printf 'Owner username (Matrix localpart): ' >&2; read -r user
  printf 'Owner password (input hidden): ' >&2; read -rs pass; printf '\n' >&2
  capture_admin_token "$domain" "$user" "$pass"
  warn "if the old token was leaked, also log out that device via the Synapse admin API"
}

# _rotate_stash_previous KEY -- keep the current value as KEY_PREVIOUS so mm-core can
# still decrypt what it wrote and re-encrypt it under the new key at startup.
# Never overwrites a KEY_PREVIOUS that is already set: it can be the only key some
# stored secrets are still under (an earlier rotation that did not finish). mm-core
# counts only rows on the CONFIGURED previous key, so replacing it would hide those
# rows from the re-encryption wait, and dropping _PREVIOUS afterwards would lose them.
_rotate_stash_previous() {
  local key="$1" cur
  [ -z "$(read_secret "${key}_PREVIOUS")" ] \
    || die "rotate: ${key}_PREVIOUS is already set; refusing to overwrite it (an earlier rotation did not finish; 'mmctl rotate $key' resumes it)"
  cur="$(read_secret "$key")"
  [ -n "$cur" ] || die "rotate: $key is empty — nothing to rotate from"
  _upsert_secret "${key}_PREVIOUS" "$cur"
}

# Last settings API body seen by _rotate_settings_wait_reencrypted, kept so the caller
# can say why the wait failed. The API never returns secret values.
_ROTATE_SETTINGS_LAST_BODY=""

# _rotate_settings_wait_reencrypted -- poll mm-core's settings API from inside the
# container (admin token on stdin, never argv) until re-encryption under the new key
# is CONFIRMED. Tries: MM_ROTATE_VERIFY_TRIES (30); pause: MM_ROTATE_VERIFY_SLEEP (2s).
#
# Requires "safe_mode":false, "encryption_key_configured":true, "rows_on_previous_key":0
# (followed by "," or "}", so a longer number never counts as 0; mm-core serialises the
# view as compact JSON with secret_problems right after rows_on_previous_key) AND
# "secret_problems":[].
# A stored secret mm-core cannot decrypt, e.g. one under a key that is neither current
# nor configured as previous, shows up only in secret_problems, never in the row count.
# rows_on_previous_key alone is not proof: mm-core also reports 0 rows when it has
# no settings key loaded at all (for example a bad MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS
# after this rotation's phase 1). Accepting rows==0 on its own here would make the
# caller drop KEY_PREVIOUS, the only key that can still decrypt the secrets mm-core
# has not actually re-encrypted yet.
# An empty secret_problems is not proof either while mm-core is in safe mode: with
# MM_SETTINGS_SAFE_MODE set it skips the stored settings, so it never finds a secret it
# cannot decrypt, and in automatic safe mode the stored values are not in use. A body
# without the safe_mode field never passes.
_rotate_settings_wait_reencrypted() {
  local tok body i tries="${MM_ROTATE_VERIFY_TRIES:-30}"
  _ROTATE_SETTINGS_LAST_BODY=""
  tok="$(read_secret MM_ADMIN_TOKEN)"
  for ((i = 0; i < tries; i++)); do
    body="$(printf 'Authorization: Bearer %s\n' "$tok" \
      | _rotate_dc exec -T mm-core curl -sf -H @- http://localhost:6168/_mm/admin/v1/settings 2>/dev/null || true)"
    _ROTATE_SETTINGS_LAST_BODY="$body"
    # Matched in the shell, never via `printf | grep -q`: the body carries the whole
    # setting schema, and when grep -q exits at a matching line while printf is still
    # writing, printf dies of SIGPIPE and pipefail turns the match into "no match".
    if [[ $body == *'"safe_mode":false'* ]] \
       && [[ $body == *'"encryption_key_configured":true'* ]] \
       && [[ $body == *'"rows_on_previous_key":0,'* || $body == *'"rows_on_previous_key":0}'* ]] \
       && [[ $body == *'"secret_problems":[]'* ]]; then
      return 0
    fi
    sleep "${MM_ROTATE_VERIFY_SLEEP:-2}"
  done
  return 1
}

# _rotate_settings_wait_cause BODY KEY -- one line saying why BODY does not confirm the
# re-encryption of KEY. Prints fixed text and a row count only, never the body.
_rotate_settings_wait_cause() {
  local body="$1" key="$2" rows="" re='"rows_on_previous_key":([0-9]+)'
  if [ -z "$body" ]; then
    echo "the settings API gave no answer (mm-core unreachable, or the admin token was rejected)"
    return 0
  fi
  if [[ $body == *'"encryption_key_configured":false'* ]]; then
    echo "mm-core did not load the new $key (encryption_key_configured is false)"
    return 0
  fi
  if [[ $body == *'"safe_mode":true'* ]]; then
    if [[ $body == *'"break_glass":true'* ]]; then
      echo "MM_SETTINGS_SAFE_MODE is set, so mm-core skips the stored settings and cannot confirm they decrypt under the new $key; remove it from $MM_ROOT/.env first"
    else
      echo "mm-core is in safe mode, so the stored settings are not in use (the reason is shown under Settings in the Operator Console)"
    fi
    return 0
  fi
  if [[ $body =~ $re ]]; then rows="${BASH_REMATCH[1]}"; fi
  if [ -n "$rows" ] && [ "$rows" -gt 0 ]; then
    echo "re-encryption not finished: $rows stored secret(s) still on the previous key"
  elif [[ $body == *'"secret_problems":['* && $body != *'"secret_problems":[]'* ]]; then
    echo "mm-core reports secret problems (see Settings in the Operator Console); ${key}_PREVIOUS kept"
  else
    echo "the settings API answered without the expected fields"
  fi
}

# _rotate_die_unhealthy KEY ALTER [WHAT] -- a recreate failed (docker compose itself
# failed, WHAT says so) or the stack did not come back healthy after it. For the
# settings key, restoring the phase-0 .env.secrets would throw away the key mm-core
# may already have re-encrypted everything under, so its message gives the reverse
# procedure instead of the uniform rollback.
_rotate_die_unhealthy() {
  local key="$1" alter="$2" what="${3:-rotation left the stack unhealthy}" phase0 after
  if [ "$alter" != reencrypt_settings ]; then
    die "$what — see the rollback section of deploy/docs/rotation-runbooks.md"
  fi
  if [ -n "$_ROTATE_BACKUP_DIR" ]; then
    phase0="$_ROTATE_BACKUP_DIR/.env.secrets"
    after="$_ROTATE_BACKUP_DIR/.env.secrets.after-generate"
  else   # a resumed run: the backups belong to the run that generated the new key
    phase0="$MM_ROOT/rotate-backups/<ts>/.env.secrets"
    after="$MM_ROOT/rotate-backups/<ts>/.env.secrets.after-generate (from the run that generated the new key)"
  fi
  die "$what. Do NOT restore $phase0: stored secrets may already be encrypted under the new $key, and that file does not hold it. The new key is also saved in $after. To go back to the old key: in $MM_ROOT/.env.secrets set $key to the old value (from ${key}_PREVIOUS if still set, else the $key line of $phase0) and ${key}_PREVIOUS to the current value, run 'mmctl start' and let mm-core re-encrypt back (it recreates mm-core with the edited file); 'mmctl rotate $key' then resumes and drops ${key}_PREVIOUS. See the $key entry in deploy/docs/rotation-runbooks.md"
}

_rotate_confirm() {
  local key="$1" note="$2" reply
  warn "rotating $key — expected window: $note"
  printf 'Proceed? [y/N] ' >&2
  read -r reply
  case "$reply" in y|Y|yes|YES) : ;; *) die "rotation aborted (use --yes to skip this prompt)" ;; esac
}

# rotate_secret KEY DRY YES -- the engine. DRY=1 prints the plan and exits 0
# without reading or writing any secret value.
rotate_secret() {
  local key="$1" dry="${2:-0}" yes="${3:-0}"
  local partner plan len files render alter restarts note new svcs resume=0

  if partner="$(_rotate_paired "$key")"; then
    die "$key is a paired literal — rotate it together with $partner (see the $partner runbook in deploy/docs/rotation-runbooks.md)"
  fi
  plan="$(_rotate_plan "$key")" \
    || die "unknown or non-rotatable secret: $key (try: mmctl rotate --list)"
  IFS='|' read -r len files render alter restarts note <<<"$plan"

  if [ "$dry" -eq 1 ]; then
    _rotate_print_plan "$key" "$plan"
    return 0
  fi

  set +x   # belt-and-braces: never trace secret values
  require_cmd docker; require_cmd openssl
  if [ ! -f "$MM_ROOT/.env" ] || [ ! -f "$MM_ROOT/.env.secrets" ]; then
    die "rotate: $MM_ROOT/.env(.secrets) not found — is this an installed host?"
  fi
  grep -q "^${key}=" "$MM_ROOT/.env.secrets" \
    || die "rotate: $key not present in $MM_ROOT/.env.secrets"
  if [ "$alter" = reencrypt_settings ]; then
    compose_passes_settings_key \
      || die "rotate: $MM_ROOT/docker-compose.yml does not pass MM_SETTINGS_ENCRYPTION_KEY to mm-core; re-run install.sh to refresh it before rotating"
    compose_passes_settings_previous_key \
      || die "rotate: $MM_ROOT/docker-compose.yml does not pass MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS to mm-core, so it could not decrypt the secrets still under the old key; re-run install.sh to refresh it before rotating"
    # A set _PREVIOUS means an earlier run stopped after phase 1 (up failed, the wait
    # timed out, Ctrl-C). Starting over would stash the unfinished new key as
    # _PREVIOUS and strand every secret still on the old one, so finish that run.
    if [ -n "$(read_secret "${key}_PREVIOUS")" ]; then
      resume=1
      log "rotate: resuming an unfinished rotation of $key: keeping ${key}_PREVIOUS, not generating a new key"
    fi
  fi
  if [ "$yes" -ne 1 ]; then _rotate_confirm "$key" "$note"; fi

  _ROTATE_BACKUP_DIR=""
  if [ "$resume" -eq 0 ]; then
    _rotate_backup                                          # phase 0

    if [ "$alter" = capture_admin ]; then                   # phase 1
      _rotate_capture_admin
    else
      if [ "$alter" = reencrypt_settings ]; then _rotate_stash_previous "$key"; fi
      new="$(openssl rand -hex "$((len / 2))")"
      _upsert_secret "$key" "$new"
      log "rotate: new $key written to .env.secrets"
    fi
    _rotate_backup_after_generate
  fi

  if [ "$files" = yes ]; then write_secret_files; fi        # phase 2
  if [ "$render" = yes ]; then render_templates; fi
  case "$alter" in
    alter_synapse)  printf '%s\n' "$new" | _alter_role_password postgres    synapse  synapse  synapse ;;
    alter_mm_admin) printf '%s\n' "$new" | _alter_role_password mm-postgres postgres postgres mm_admin ;;
    alter_mm_app)   printf '%s\n' "$new" | _alter_role_password mm-postgres postgres postgres mm_app ;;
  esac

  if [ "$restarts" != "-" ]; then                           # phase 3
    IFS=',' read -ra svcs <<<"$restarts"
    log "rotate: recreating in order: ${svcs[*]}"
    # A compose failure takes the same way out as an unhealthy stack: under set -e it
    # would otherwise end the run with compose's own error and no way back.
    _rotate_dc up -d --force-recreate "${svcs[@]}" \
      || _rotate_die_unhealthy "$key" "$alter" "docker compose could not recreate ${svcs[*]} (see its error above)"
    wait_healthy matrixmedia 300 || _rotate_die_unhealthy "$key" "$alter"
  fi

  if [ "$alter" = reencrypt_settings ]; then                # phase 3b
    if ! _rotate_settings_wait_reencrypted; then
      die "rotate: re-encryption under the new $key not confirmed: $(_rotate_settings_wait_cause "$_ROTATE_SETTINGS_LAST_BODY" "$key"). ${key}_PREVIOUS left in place (nothing is lost); read the mm-core log for 'settings:' lines, fix the cause, then re-run 'mmctl rotate $key' to resume"
    fi
    _remove_secret "${key}_PREVIOUS"
    _rotate_dc up -d --force-recreate mm-core \
      || _rotate_die_unhealthy "$key" "$alter" "docker compose could not recreate mm-core (see its error above)"
    wait_healthy matrixmedia 300 || _rotate_die_unhealthy "$key" "$alter"
    log "rotate: previous settings key removed"
  fi

  if [ "$resume" -eq 1 ]; then                              # phase 4/5
    log "rotate: finished an earlier rotation of $key; no new key was generated."
  else
    log "rotated $key."
  fi
  log "verify now: mmctl doctor + the $key probes in deploy/docs/rotation-runbooks.md"
  log "then confirm the OLD value no longer works (negative probe) and purge $MM_ROOT/rotate-backups/ after the soak window"
}

# rotate_cmd [ARGS...] -- argv parsing for the mmctl dispatch arm.
rotate_cmd() {
  local key="" dry=0 yes=0 list=0 a
  for a in "$@"; do
    case "$a" in
      --dry-run) dry=1 ;;
      --yes)     yes=1 ;;
      --list)    list=1 ;;
      -*) echo "usage: mmctl rotate <SECRET_NAME> [--dry-run] [--yes] | mmctl rotate --list" >&2; return 1 ;;
      *)  if [ -n "$key" ]; then
            echo "usage: mmctl rotate <SECRET_NAME> [--dry-run] [--yes] | mmctl rotate --list" >&2; return 1
          fi
          key="$a" ;;
    esac
  done
  if [ "$list" -eq 1 ]; then rotate_list; return 0; fi
  if [ -z "$key" ]; then
    echo "usage: mmctl rotate <SECRET_NAME> [--dry-run] [--yes] | mmctl rotate --list" >&2
    return 1
  fi
  rotate_secret "$key" "$dry" "$yes"
}
