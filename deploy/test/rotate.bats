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
