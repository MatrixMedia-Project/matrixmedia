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

# Compose wrapper pinned to both env files + the project name (mirrors
# stack_up in up.sh / the DC array in mmctl).
_rotate_dc() {
  docker compose --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -p matrixmedia "$@"
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
    SYNAPSE_REGISTRATION_SECRET) echo "64|yes|yes|none|synapse,mm-core|signup endpoint errors for one synapse+mm-core recreate" ;;
    SYNAPSE_MACAROON_SECRET)     echo "64|no|yes|none|synapse|compromise-only rotation; some login flows re-auth" ;;
    SYNAPSE_FORM_SECRET)         echo "64|no|yes|none|synapse|one synapse restart; low impact" ;;
    POSTGRES_SYNAPSE_PASS)       echo "32|no|yes|alter_synapse|synapse|ALTER ROLE synapse then recreate synapse (~30s; clients retry, federation queues)" ;;
    POSTGRES_APP_ADMIN_PASS)     echo "32|yes|yes|alter_mm_admin|mm-core|ALTER ROLE mm_admin then recreate mm-core (~10-30s API gap; DB uninterrupted)" ;;
    POSTGRES_APP_PASS)           echo "32|yes|yes|alter_mm_app|-|no live consumer in the compose file; ALTER + secret-file refresh only" ;;
    MINIO_ROOT_PASSWORD)         echo "32|no|no|none|minio|one minio recreate; no user-visible effect" ;;
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
    MINIO_ROOT_USER) echo "MINIO_ROOT_PASSWORD" ;;
    TURN_USER)       echo "TURN_PASS" ;;
    *) return 1 ;;
  esac
}

# Generated but consumed by nothing (verified: no compose/template reference).
# Do NOT rotate dead entropy — flag it for removal from generate_secrets.
_rotate_dead() {
  case "$1" in
    TURN_SECRET|GRAFANA_ADMIN_PASSWORD) return 0 ;;
    *) return 1 ;;
  esac
}

# Rotatable keys, in inventory order (drives --list and the map-sanity test).
_rotate_keys() {
  echo "LK_API_SECRET MM_AS_TOKEN MM_HS_TOKEN MM_ADMIN_TOKEN MM_JWT_SIGNING_KEY \
MM_SWITCH_AUTH_SECRET MM_SIGNUP_IP_HASH_PEPPER SYNAPSE_REGISTRATION_SECRET \
SYNAPSE_MACAROON_SECRET SYNAPSE_FORM_SECRET POSTGRES_SYNAPSE_PASS \
POSTGRES_APP_ADMIN_PASS POSTGRES_APP_PASS MINIO_ROOT_PASSWORD REDIS_PASSWORD \
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
       "MINIO_ROOT_USER -> MINIO_ROOT_PASSWORD, TURN_USER -> TURN_PASS"
  echo "Dead secrets (generated, consumed by nothing — flagged for removal," \
       "nothing to rotate): TURN_SECRET, GRAFANA_ADMIN_PASSWORD"
  echo "Operator-supplied (rotate at the provider, paste into $MM_ROOT/.env," \
       "then recreate mm-core): MM_STRIPE_SECRET_KEY, MM_STRIPE_WEBHOOK_SECRET," \
       "MM_LNBITS_INVOICE_KEY, MM_LNBITS_ADMIN_KEY"
  echo "Runbooks: deploy/docs/rotation-runbooks.md"
}

# _rotate_print_plan KEY PLAN -- the --dry-run output: ordered phases, no
# secret value is ever read or written here (pure shell on the static map).
_rotate_print_plan() {
  local key="$1" len files render alter restarts note
  IFS='|' read -r len files render alter restarts note <<<"$2"
  echo "rotation plan for $key (dry-run, nothing changed):"
  echo "  phase 0  PRECHECK   backup .env.secrets + secrets/ + config/ -> $MM_ROOT/rotate-backups/<ts>/ (mode 700)"
  if [ "$alter" = capture_admin ]; then
    echo "  phase 1  GENERATE   none — re-login as the server owner captures a fresh Synapse token (capture_admin_token)"
  else
    echo "  phase 1  GENERATE   openssl rand -hex $len -> _upsert_secret $key (value never logged)"
  fi
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
  echo "  phase 4  VERIFY     wait_healthy + mmctl doctor + the $key probes in deploy/docs/rotation-runbooks.md"
  echo "  phase 5  INVALIDATE negative probe with the old value; purge the backup after the soak window"
  echo "  expected window: $note"
}

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
  log "rotate: backup at $dir (contains the OLD secret values — purge after the soak window)"
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
  local partner plan len files render alter restarts note new svcs

  if _rotate_dead "$key"; then
    die "$key is generated but consumed by nothing (dead secret) — nothing to rotate; it is flagged for removal from generate_secrets (see deploy/docs/secrets-inventory.md)"
  fi
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
  if [ "$yes" -ne 1 ]; then _rotate_confirm "$key" "$note"; fi

  _rotate_backup                                            # phase 0

  if [ "$alter" = capture_admin ]; then                     # phase 1
    _rotate_capture_admin
  else
    new="$(openssl rand -hex "$((len / 2))")"
    _upsert_secret "$key" "$new"
    log "rotate: new $key written to .env.secrets"
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
    _rotate_dc up -d --force-recreate "${svcs[@]}"
    wait_healthy matrixmedia 300 \
      || die "rotation left the stack unhealthy — see the rollback section of deploy/docs/rotation-runbooks.md"
  fi

  log "rotated $key."                                       # phase 4/5
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
