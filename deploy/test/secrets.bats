load helper
setup() { setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/secrets.sh"; }
teardown() { teardown_tmp; }

@test "gen_secret writes a hex value of requested length" {
  gen_secret FOO 32
  grep -q '^FOO=[0-9a-f]\{32\}$' "$MM_ROOT/.env.secrets"
}
@test "gen_secret is idempotent — never rotates an existing value" {
  gen_secret FOO 32; local first; first="$(grep '^FOO=' "$MM_ROOT/.env.secrets")"
  gen_secret FOO 32; local second; second="$(grep '^FOO=' "$MM_ROOT/.env.secrets")"
  [ "$first" = "$second" ]
}
@test "generate_secrets produces the full required key set" {
  generate_secrets
  for k in LK_API_KEY LK_API_SECRET MM_AS_TOKEN MM_HS_TOKEN MM_ADMIN_TOKEN \
           MM_JWT_SIGNING_KEY MM_SWITCH_AUTH_SECRET MM_SIGNUP_IP_HASH_PEPPER \
           MM_SETTINGS_ENCRYPTION_KEY \
           SYNAPSE_REGISTRATION_SECRET SYNAPSE_MACAROON_SECRET SYNAPSE_FORM_SECRET \
           POSTGRES_SYNAPSE_PASS POSTGRES_APP_ADMIN_PASS POSTGRES_APP_PASS \
           POSTGRES_FLEET_RUNNER_PASS \
           REDIS_PASSWORD TURN_PASS; do
    grep -q "^${k}=" "$MM_ROOT/.env.secrets" || { echo "missing $k"; return 1; }
  done
}
@test "env.secrets is mode 0600" {
  generate_secrets
  [ "$(file_mode "$MM_ROOT/.env.secrets")" = "600" ]
}

@test "write_secret_files creates all 4 secret files" {
  generate_secrets
  write_secret_files
  [ -f "$MM_ROOT/secrets/mm_db_app_password" ]
  [ -f "$MM_ROOT/secrets/mm_db_admin_password" ]
  [ -f "$MM_ROOT/secrets/synapse_registration_shared_secret" ]
  [ -f "$MM_ROOT/secrets/signup_ip_hash_pepper" ]
}

@test "write_secret_files secret files have mode 0600" {
  generate_secrets
  write_secret_files
  for name in mm_db_app_password mm_db_admin_password synapse_registration_shared_secret signup_ip_hash_pepper; do
    [ "$(file_mode "$MM_ROOT/secrets/$name")" = "600" ]
  done
}

@test "write_secret_files secret files have correct content" {
  generate_secrets
  write_secret_files
  app_pass="$(grep '^POSTGRES_APP_PASS=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  admin_pass="$(grep '^POSTGRES_APP_ADMIN_PASS=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  reg_secret="$(grep '^SYNAPSE_REGISTRATION_SECRET=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  pepper="$(grep '^MM_SIGNUP_IP_HASH_PEPPER=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  [ "$(cat "$MM_ROOT/secrets/mm_db_app_password")" = "$app_pass" ]
  [ "$(cat "$MM_ROOT/secrets/mm_db_admin_password")" = "$admin_pass" ]
  [ "$(cat "$MM_ROOT/secrets/synapse_registration_shared_secret")" = "$reg_secret" ]
  [ "$(cat "$MM_ROOT/secrets/signup_ip_hash_pepper")" = "$pepper" ]
}

@test "write_secret_files secret files have no trailing newline" {
  generate_secrets
  write_secret_files
  for name in mm_db_app_password mm_db_admin_password synapse_registration_shared_secret signup_ip_hash_pepper; do
    f="$MM_ROOT/secrets/$name"
    # File size must equal length of the value (no trailing newline byte)
    val="$(grep "^${name//_db_app_password/POSTGRES_APP_PASS}" "$MM_ROOT/.env.secrets" 2>/dev/null || true)"
    # Check that the last byte is NOT a newline (0x0a)
    last="$(tail -c 1 "$f" | xxd -p 2>/dev/null || tail -c 1 "$f" | od -An -tx1 | tr -d ' \n')"
    [ "$last" != "0a" ] || { echo "trailing newline in $name"; return 1; }
  done
}

@test "read_secret returns value, empty for absent key, survives pipefail" {
  f="$BATS_TEST_TMPDIR/sec"; printf 'A=1\nMM_OWNER_BOOTSTRAP_PASS=hunter2\n' > "$f"
  run read_secret MM_OWNER_BOOTSTRAP_PASS "$f"; [ "$status" -eq 0 ]; [ "$output" = "hunter2" ]
  run read_secret NOPE "$f"; [ "$status" -eq 0 ]; [ -z "$output" ]
  run bash -c 'set -euo pipefail; source "'"$DEPLOY_ROOT"'/lib/common.sh"; source "'"$DEPLOY_ROOT"'/lib/secrets.sh"; v="$(read_secret NOPE "'"$f"'")"; echo "ok:[$v]"'
  [ "$status" -eq 0 ]; [ "$output" = "ok:[]" ]
}
@test "owner pass convergence: upsert then read_secret round-trips" {
  export MM_ROOT="$BATS_TEST_TMPDIR"; touch "$MM_ROOT/.env.secrets"
  _upsert_secret MM_OWNER_BOOTSTRAP_PASS "p1"
  run read_secret MM_OWNER_BOOTSTRAP_PASS; [ "$output" = "p1" ]
  _upsert_secret MM_OWNER_BOOTSTRAP_PASS "p2"
  run read_secret MM_OWNER_BOOTSTRAP_PASS; [ "$output" = "p2" ]
}

@test "generate_secrets adds a 64-hex MM_SETTINGS_ENCRYPTION_KEY once and never replaces it" {
  generate_secrets
  grep -q '^MM_SETTINGS_ENCRYPTION_KEY=[0-9a-f]\{64\}$' "$MM_ROOT/.env.secrets"
  first="$(grep '^MM_SETTINGS_ENCRYPTION_KEY=' "$MM_ROOT/.env.secrets")"
  generate_secrets
  [ "$first" = "$(grep '^MM_SETTINGS_ENCRYPTION_KEY=' "$MM_ROOT/.env.secrets")" ]
}

@test "_remove_secret deletes exactly one key and keeps mode 0600" {
  generate_secrets
  _upsert_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS abc
  _remove_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS
  run grep -q '^MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS=' "$MM_ROOT/.env.secrets"
  [ "$status" -eq 1 ]                          # 1 = no match (2 would be a read error)
  grep -q '^MM_SETTINGS_ENCRYPTION_KEY=' "$MM_ROOT/.env.secrets"
  [ "$(file_mode "$MM_ROOT/.env.secrets")" = "600" ]
}

# If grep cannot read the file, the old code wrote an empty (or one-line) file over it:
# every other secret gone. Both writers must fail and leave the file alone.
@test "_upsert_secret and _remove_secret refuse an unreadable .env.secrets and leave it untouched" {
  [ "$(id -u)" -eq 0 ] && skip "root can read a mode-000 file"
  generate_secrets
  _upsert_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS abc
  before="$(cat "$MM_ROOT/.env.secrets")"
  chmod 000 "$MM_ROOT/.env.secrets"
  run _upsert_secret MM_ADMIN_TOKEN replacement
  upsert_status="$status"
  run _remove_secret MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS
  remove_status="$status"
  chmod 600 "$MM_ROOT/.env.secrets"
  [ "$upsert_status" -ne 0 ]
  [ "$remove_status" -ne 0 ]
  [ "$before" = "$(cat "$MM_ROOT/.env.secrets")" ]
  # no temp file left next to it
  [ -z "$(find "$MM_ROOT" -maxdepth 1 -name '.env.secrets.*' -print)" ]
}

@test "compose_passes_settings_key tells old compose files from new ones" {
  printf 'services:\n  mm-core:\n    environment:\n      MM_ADMIN_TOKEN: x\n' > "$MM_ROOT/docker-compose.yml"
  run compose_passes_settings_key
  [ "$status" -ne 0 ]
  echo '      MM_SETTINGS_ENCRYPTION_KEY: ${MM_SETTINGS_ENCRYPTION_KEY:-}' >> "$MM_ROOT/docker-compose.yml"
  run compose_passes_settings_key
  [ "$status" -eq 0 ]
}

@test "compose_passes_alert_token tells old compose files from new ones" {
  printf 'services:\n  mm-core:\n    environment:\n      # MM_ALERT_WEBHOOK_TOKEN: in a comment\n' > "$MM_ROOT/docker-compose.yml"
  run compose_passes_alert_token
  [ "$status" -ne 0 ]
  echo '      MM_ALERT_WEBHOOK_TOKEN: ${MM_ALERT_WEBHOOK_TOKEN:-}' >> "$MM_ROOT/docker-compose.yml"
  run compose_passes_alert_token
  [ "$status" -eq 0 ]
}

@test "the compose template hands mm-core the alert webhook token, empty unless set" {
  grep -q '^      MM_ALERT_WEBHOOK_TOKEN: ${MM_ALERT_WEBHOOK_TOKEN:-}$' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

# The fleet runner is the only process that holds cloud credentials: it publishes nothing,
# reaches only Postgres, and logs in as its own role with its own generated password.
@test "the compose template runs mm-fleet-runner as its own unpublished service on internal and mm-db-net only" {
  svc="$(awk '/^  mm-fleet-runner:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
  [ -n "$svc" ]
  [[ "$svc" == *"image: \${MM_REGISTRY}/matrixmedia-mm-core:\${MM_VERSION}"* ]] || { echo "not the mm-core image"; return 1; }
  [[ "$svc" == *"entrypoint: [mm-fleet-runner]"* ]] || { echo "wrong entrypoint"; return 1; }
  [[ "$svc" == *"command: [run]"* ]] || { echo "wrong command"; return 1; }
  [[ "$svc" == *"@mm-postgres:5432/matrixmedia"* ]] || { echo "does not talk to mm-postgres"; return 1; }
  [[ "$svc" == *"mm_fleet_runner:\${POSTGRES_FLEET_RUNNER_PASS}@"* ]] || { echo "not its own role and password"; return 1; }
  [[ "$svc" != *"POSTGRES_APP_ADMIN_PASS"* ]] || { echo "must never hold the mm_admin password"; return 1; }
  # no port of any kind, and no traefik exposure
  run grep -nE '^[[:space:]]+(ports|expose|labels):' <<<"$svc"
  [ "$status" -eq 1 ] || { echo "$output"; return 1; }
  # networks: exactly internal and mm-db-net
  nets="$(awk '/^    networks:$/{on=1; next} on && /^      - /{print $2; next} on{exit}' <<<"$svc" | tr '\n' ' ')"
  [ "$nets" = "internal mm-db-net " ] || { echo "networks: $nets"; return 1; }
  grep -q '^  mm-fleet-tfvars: {}$' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

# Nothing in the installer applies mm_fleet_runner_role.sql, sets that role's password or
# creates the key directory, so as a default service the runner would crash-loop on every
# fresh install. It is opt-in behind the "fleet" compose profile: `up -d` must not start it.
@test "mm-fleet-runner is opt-in: it carries the fleet compose profile and no other" {
  svc="$(awk '/^  mm-fleet-runner:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
  [ -n "$svc" ]
  profiles="$(grep -E '^    profiles:' <<<"$svc")"
  [ "$profiles" = '    profiles: ["fleet"]' ] || { echo "profiles: ${profiles:-<none>}"; return 1; }
}

@test "mm-fleet-runner serves /metrics on the docker network and is health-checked by it" {
  svc="$(awk '/^  mm-fleet-runner:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
  grep -q 'MM_FLEET_RUNNER_LISTEN: 0.0.0.0:9465' <<<"$svc"
  grep -q 'http://127.0.0.1:9465/metrics' <<<"$svc"
  ! grep -qE '^    ports:' <<<"$svc"
}

# mm-core and the runner both resolve the fleet mode and the orphan grace from env when no
# settings row exists, so one .env value has to reach both, with the same default. A
# non-empty default: mm-core treats an empty MM_FLEET_MODE as an error, not as absent.
@test "mm-core and mm-fleet-runner both get MM_FLEET_MODE and MM_FLEET_ORPHAN_MIN_AGE_SECS" {
  for name in mm-core mm-fleet-runner; do
    svc="$(awk -v n="$name" '$0 == "  " n ":" {on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
    [ -n "$svc" ]
    grep -qxF '      MM_FLEET_MODE: ${MM_FLEET_MODE:-frozen}' <<<"$svc" \
      || { echo "$name: MM_FLEET_MODE is not passed with the frozen default"; return 1; }
    grep -qxF '      MM_FLEET_ORPHAN_MIN_AGE_SECS: ${MM_FLEET_ORPHAN_MIN_AGE_SECS:-1800}' <<<"$svc" \
      || { echo "$name: MM_FLEET_ORPHAN_MIN_AGE_SECS is not passed with the 1800 default"; return 1; }
  done
}

@test ".env.example documents MM_FLEET_MODE and MM_FLEET_ORPHAN_MIN_AGE_SECS, commented out" {
  grep -qx '# MM_FLEET_MODE=frozen' "$DEPLOY_ROOT/.env.example"
  grep -qx '# MM_FLEET_ORPHAN_MIN_AGE_SECS=1800' "$DEPLOY_ROOT/.env.example"
}

# From Task 22 the runner needs V042 (keyed on mm_fleet_requests.params), not only V041.
@test "the comment above mm-fleet-runner names the migration it waits for: V042" {
  c="$(awk '/^  mm-fleet-runner:$/{exit} /^  # ── mm-fleet-runner/{on=1} on' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
  [[ "$c" == *"before V042 exists"* ]] || { echo "the comment does not say V042"; return 1; }
  [[ "$c" != *"V041"* ]] || { echo "the comment still says V041"; return 1; }
}

# The tfvars volume is created from the image's own /var/lib/mm-fleet, so the directory must
# exist there and belong to the unprivileged user the runner runs as.
@test "the image creates /var/lib/mm-fleet and gives it to the matrixmedia user" {
  df="$DEPLOY_ROOT/../infra/docker/Dockerfile"
  grep -qE 'mkdir -p /data /etc/matrixmedia /var/lib/mm-fleet( |$)' "$df"
  grep -qE 'chown matrixmedia:matrixmedia /data /etc/matrixmedia /var/lib/mm-fleet( |$)' "$df"
}

@test "the runner role SQL says to re-run it after every upgrade" {
  head -5 "$DEPLOY_ROOT/sql/mm_fleet_runner_role.sql" | grep -q 'Re-run after every upgrade'
}

@test "runbook D covers GPU servers: release, runner down, the guard override and the role re-run" {
  d="$(awk '/^## Runbook D/{on=1; print; next} on && /^## /{exit} on' "$DEPLOY_ROOT/docs/rotation-runbooks.md")"
  [ -n "$d" ]
  for s in "MMFleetRunnerStaleWithRentedNodes" "MM_FLEET_FORCE=1" "mm_fleet_runner_role.sql" "mm-fleet-runner:9465" \
           "MM_FLEET_FORCE=1 mmctl restart mm-fleet-runner" "fails closed" "mmctl rotate POSTGRES_FLEET_RUNNER_PASS" \
           "stop|restart|update|upgrade|restore|uninstall" \
           "MMFleetReapedByDeadline" "MMFleetOrphanDestroyed" 'task="fleet_loop"' \
           "DELETE FROM mm_fleet_desired WHERE mm_node_id = '<node id>'; UPDATE mm_fleet_nodes SET state = 'gone'" \
           "mm-fleet-runner rotate-key" "needs re-entry"; do
    [[ "$d" == *"$s"* ]] || { echo "runbook D does not mention: $s"; return 1; }
  done
}

# The volume fix runs as root against the image the stack already has, not a floating tag, and
# defines every variable it uses.
@test "runbook D5 chowns the tfvars volume with the stack's own mm-core image, before the new runner starts" {
  d="$(awk '/^## Runbook D/{on=1; print; next} on && /^## /{exit} on' "$DEPLOY_ROOT/docs/rotation-runbooks.md")"
  step="$(awk '/^\*\*D5\./{on=1} /^\*\*D6\./{on=0} on' <<<"$d")"
  [ -n "$step" ]
  [[ "$step" != *alpine* ]] || { echo "D5 pulls a floating alpine"; return 1; }
  [[ "$step" == *'--user 0 --entrypoint chown'* ]] || { echo "D5 does not run chown as root"; return 1; }
  [[ "$step" == *'"$MM_REGISTRY/matrixmedia-mm-core:$MM_VERSION" matrixmedia:matrixmedia /v'* ]] \
    || { echo "D5 does not use the local mm-core image"; return 1; }
  [[ "$step" == *'. "$MM_ROOT/versions.env"; . "$MM_ROOT/.env"'* ]] || { echo "D5 does not define MM_REGISTRY and MM_VERSION"; return 1; }
  [[ "$step" == *'before the new runner first starts'* ]] || { echo "D5 does not say when to run it"; return 1; }
  # it must really run, with those variables defined: execute its bash block against a docker stub
  mkdir -p "$MM_ROOT/bin"
  printf '#!/bin/sh\nprintf "%%s\\n" "$*" > "$MM_ROOT/docker-run-args"\n' > "$MM_ROOT/bin/docker"
  chmod +x "$MM_ROOT/bin/docker"
  printf 'MM_REGISTRY=reg.example\nMM_VERSION=1.2.3\n' > "$MM_ROOT/versions.env"
  : > "$MM_ROOT/.env"
  block="$(awk '/^```bash$/{on=1; next} /^```$/{on=0} on' <<<"$step")"
  [ -n "$block" ]
  PATH="$MM_ROOT/bin:$PATH" run bash -c "$block"
  [ "$status" -eq 0 ] || { echo "$output"; return 1; }
  [ "$(cat "$MM_ROOT/docker-run-args")" = "run --rm --user 0 --entrypoint chown -v matrixmedia_mm-fleet-tfvars:/v reg.example/matrixmedia-mm-core:1.2.3 matrixmedia:matrixmedia /v" ] \
    || { cat "$MM_ROOT/docker-run-args"; return 1; }
}

# The "fleet" profile is switched on by MM_FLEET_RUNNER=true in .env, through the same
# profiles_from_env that compose_env_files exports as COMPOSE_PROFILES for every mmctl verb.
@test "profiles_from_env enables fleet only for MM_FLEET_RUNNER=true, alone or with demo" {
  source "$DEPLOY_ROOT/lib/common.sh"
  f="$MM_ROOT/.env"
  # absent, false, or no .env at all: no fleet
  printf 'MM_DOMAIN=example.com\n' > "$f"
  [ -z "$(profiles_from_env "$f")" ]
  printf 'MM_FLEET_RUNNER=false\n' > "$f"
  [ -z "$(profiles_from_env "$f")" ]
  printf '# MM_FLEET_RUNNER=true\n' > "$f"
  [ -z "$(profiles_from_env "$f")" ]
  [ -z "$(profiles_from_env "$MM_ROOT/nonexistent")" ]
  # demo alone is unchanged
  printf 'MM_DEMO_MODE=true\nMM_FLEET_RUNNER=false\n' > "$f"
  [ "$(profiles_from_env "$f")" = "demo" ]
  # the switch alone
  printf 'MM_FLEET_RUNNER=true\n' > "$f"
  [ "$(profiles_from_env "$f")" = "fleet" ]
  # both: comma separated, as COMPOSE_PROFILES wants
  printf 'MM_DEMO_MODE=true\nMM_FLEET_RUNNER=true\n' > "$f"
  [ "$(profiles_from_env "$f")" = "demo,fleet" ]
}

@test "compose_env_files exports COMPOSE_PROFILES=fleet from .env, so mmctl start/stop/rotate see the runner" {
  source "$DEPLOY_ROOT/lib/common.sh"
  printf 'MM_FLEET_RUNNER=true\n' > "$MM_ROOT/.env"
  compose_env_files
  [ "$COMPOSE_PROFILES" = "fleet" ]
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  compose_env_files
  [ -z "$COMPOSE_PROFILES" ]
}

@test ".env.example documents MM_FLEET_RUNNER and never ships it enabled" {
  grep -q 'MM_FLEET_RUNNER' "$DEPLOY_ROOT/.env.example"
  run grep -E '^[[:space:]]*MM_FLEET_RUNNER=true' "$DEPLOY_ROOT/.env.example"
  [ "$status" -eq 1 ]
}

@test "no template routes /_mm/internal through Traefik" {
  run grep -nE 'PathPrefix\(`/_mm/internal' "$DEPLOY_ROOT/docker-compose.tmpl.yml" "$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml"
  [ "$status" -eq 1 ]
  run grep -n 'mm-internal' "$DEPLOY_ROOT/docker-compose.tmpl.yml" "$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml"
  [ "$status" -eq 1 ]
}

@test "the compose template hands mm-core the settings key, the previous key and the safe-mode flag" {
  for v in MM_SETTINGS_ENCRYPTION_KEY MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS MM_SETTINGS_SAFE_MODE; do
    grep -q "^      ${v}: \${${v}:-}\$" "$DEPLOY_ROOT/docker-compose.tmpl.yml" || { echo "missing $v"; return 1; }
  done
}

# mm-core imports every non-empty secret at its first start and shows it as "Set" in the
# dashboard; an empty one is "not set" and is not imported. A made-up fallback value would
# be stored as if the operator had entered it.
@test "the compose template passes the LNbits keys through empty unless .env sets them" {
  for v in MM_LNBITS_INVOICE_KEY MM_LNBITS_ADMIN_KEY; do
    grep -q "^      ${v}: \${${v}:-}\$" "$DEPLOY_ROOT/docker-compose.tmpl.yml" || { echo "$v is not passed through empty"; return 1; }
  done
  # no key, secret, token or password in the template falls back to an invented value
  run grep -nE '\$\{[A-Z0-9_]*(KEY|SECRET|TOKEN|PASS)[A-Z0-9_]*:-[^}]' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
  [ "$status" -eq 1 ] || { echo "$output"; return 1; }
}

# A caller that checks the status (`generate_secrets || die …`) switches set -e off inside
# the function, so each step has to pass its failure on by itself; a failed openssl must
# also never leave an empty KEY= line behind, which a later run would keep forever.
# The host is an older install: every secret but the settings key exists, so the failing
# step sits in the middle and the steps after it succeed.
@test "generate_secrets reports a failed openssl to a caller that checks it and writes no empty value" {
  generate_secrets
  _remove_secret MM_SETTINGS_ENCRYPTION_KEY
  _upsert_secret MM_ADMIN_TOKEN keep-me
  mkdir -p "$MM_ROOT/bin"
  printf '#!/bin/sh\nexit 1\n' > "$MM_ROOT/bin/openssl"
  chmod +x "$MM_ROOT/bin/openssl"
  PATH="$MM_ROOT/bin:$PATH"
  rc=0
  generate_secrets 2>/dev/null || rc=$?
  [ "$rc" -ne 0 ]
  run grep -E '^[A-Z0-9_]+=$' "$MM_ROOT/.env.secrets"
  [ "$status" -eq 1 ]                          # 1 = no empty KEY= line (2 would be a read error)
  run grep -q '^MM_SETTINGS_ENCRYPTION_KEY=' "$MM_ROOT/.env.secrets"
  [ "$status" -eq 1 ]
  grep -q '^MM_ADMIN_TOKEN=keep-me$' "$MM_ROOT/.env.secrets"
}
