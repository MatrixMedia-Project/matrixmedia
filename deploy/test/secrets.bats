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

# ── mm-media-worker (ad conversion, internal spec 2026-10-11 D1) ─────────────
@test "mm-media-worker is sandboxed: no ports, only the internal ads network, its own DB role" {
  tmpl="$BATS_TEST_DIRNAME/../docker-compose.tmpl.yml"
  block="$(awk '/^  mm-media-worker:/{f=1;print;next} f&&/^  [a-z0-9-]+:/{exit} f' "$tmpl")"
  [ -n "$block" ]
  run grep -Eq '^\s+(ports|expose|labels):' <<<"$block"
  [ "$status" -ne 0 ]
  grep -q 'profiles: \["ads"\]' <<<"$block"
  grep -q 'read_only: true' <<<"$block"
  grep -q 'no-new-privileges:true' <<<"$block"
  grep -q 'cap_drop: \[ALL\]' <<<"$block"
  grep -qF 'mm_media_worker:${POSTGRES_MEDIA_WORKER_PASS}@mm-postgres' <<<"$block"
  run grep -q 'POSTGRES_APP_ADMIN_PASS' <<<"$block"
  [ "$status" -ne 0 ]
  nets="$(awk '/^    networks:/{f=1;next} f&&/^    [a-z_]+:/{exit} f&&/^      - /{print $2}' <<<"$block")"
  [ "$nets" = "mm-ads-net" ]
  grep -A3 '^  mm-ads-net:' "$tmpl" | grep -q 'internal: true'
}

@test "mm-core no longer mounts the public ads dir read-write; mm-web serves it read-only" {
  tmpl="$BATS_TEST_DIRNAME/../docker-compose.tmpl.yml"
  run grep -q '/web/ads:/opt/MatrixMedia/web/ads' "$tmpl"
  [ "$status" -ne 0 ]
  grep -q 'ad-media/incoming:/mm-ad-media/incoming' "$tmpl"
  grep -q 'web/ads:/usr/share/nginx/html/ads:ro' "$tmpl"
}

# The worker is opt-in like the fleet runner: MM_MEDIA_WORKER=true in .env selects the "ads"
# profile through the same profiles_from_env, so every mmctl verb sees the service.
@test "profiles_from_env enables ads only for MM_MEDIA_WORKER=true, alone or with the other profiles" {
  source "$DEPLOY_ROOT/lib/common.sh"
  f="$MM_ROOT/.env"
  printf 'MM_DOMAIN=example.com\n' > "$f"
  [ -z "$(profiles_from_env "$f")" ]
  printf 'MM_MEDIA_WORKER=false\n' > "$f"
  [ -z "$(profiles_from_env "$f")" ]
  printf '# MM_MEDIA_WORKER=true\n' > "$f"
  [ -z "$(profiles_from_env "$f")" ]
  printf 'MM_MEDIA_WORKER=true\n' > "$f"
  [ "$(profiles_from_env "$f")" = "ads" ]
  printf 'MM_DEMO_MODE=true\nMM_FLEET_RUNNER=true\nMM_MEDIA_WORKER=true\n' > "$f"
  [ "$(profiles_from_env "$f")" = "demo,fleet,ads" ]
}

@test "generate_secrets makes POSTGRES_MEDIA_WORKER_PASS, and .env.example never ships the worker enabled" {
  generate_secrets
  grep -q '^POSTGRES_MEDIA_WORKER_PASS=[0-9a-f]\{32\}$' "$MM_ROOT/.env.secrets"
  grep -q 'MM_MEDIA_WORKER' "$DEPLOY_ROOT/.env.example"
  run grep -E '^[[:space:]]*MM_MEDIA_WORKER=true' "$DEPLOY_ROOT/.env.example"
  [ "$status" -eq 1 ]
}

# The worker moved from mm-db-net to its own internal network, so the old subnet grant must go.
@test "pg_hba lets mm_media_worker in from mm-ads-net only" {
  hba="$DEPLOY_ROOT/templates/mm-postgres-pg_hba.tmpl.conf"
  [ "$(grep -c 'mm_media_worker' "$hba" | tr -d ' ')" = "1" ]       # the one rule, nothing else
  grep -Eq '^host[[:space:]]+matrixmedia[[:space:]]+mm_media_worker[[:space:]]+172\.29\.0\.0/24[[:space:]]+scram-sha-256$' "$hba"
  run grep -E '^host[[:space:]]+[a-z]+[[:space:]]+mm_media_worker[[:space:]]+172\.28\.' "$hba"
  [ "$status" -eq 1 ]
  # one rule for 172.29, it is the worker's, and no other rule mentions the worker or the subnet
  rules="$(grep -vE '^[[:space:]]*#' "$hba")"
  [ "$(grep -c '172\.29' <<<"$rules" | tr -d ' ')" = "1" ]
  grep -E '172\.29' <<<"$rules" | grep -q 'mm_media_worker'
  [ "$(grep -c 'mm_media_worker' <<<"$rules" | tr -d ' ')" = "1" ]
}

# Prints every pg_hba rule on stdin that lets a role other than mm_media_worker in from an address
# that overlaps mm-ads-net (172.29.0.0/24): a wider CIDR (/8, /12, /16, 0.0.0.0/0), a narrower one
# inside it, `all` or `samenet` as the address, or `all` / a list as the role. `reject` rules grant
# nothing and are skipped, `local` and IPv6 rules cannot reach the IPv4 bridge, and an address in a
# form it cannot read (a separate netmask column, a hostname) is reported too, so it fails closed.
_hba_wider_than_worker() {
  local type db user addr method rest ip len a b c d net mask lo
  while read -r type db user addr method rest; do
    case "$type" in ''|'#'*|local) continue ;; esac
    [ "$method" = "reject" ] && continue
    [ "$user" = "mm_media_worker" ] && continue
    case "$addr" in
      *:*) continue ;;
      all|samenet) echo "$type $db $user $addr $method"; continue ;;
    esac
    if [[ "$addr" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})/([0-9]{1,2})$ ]]; then
      a=${BASH_REMATCH[1]} b=${BASH_REMATCH[2]} c=${BASH_REMATCH[3]} d=${BASH_REMATCH[4]} len=${BASH_REMATCH[5]}
      net=$(( (a << 24) | (b << 16) | (c << 8) | d ))
      lo=$len; [ "$lo" -gt 24 ] && lo=24            # overlap: compare on the shorter of the two prefixes
      mask=$(( lo == 0 ? 0 : (0xFFFFFFFF << (32 - lo)) & 0xFFFFFFFF ))
      if [ $(( net & mask )) -eq $(( ((172 << 24) | (29 << 16)) & mask )) ]; then
        echo "$type $db $user $addr $method"
      fi
    else
      echo "$type $db $user $addr $method   # unreadable address"
    fi
  done
}

@test "the pg_hba checker flags every wider or overlapping rule for another role (not vacuous)" {
  for bad in \
    'host matrixmedia mm_app 172.16.0.0/12 scram-sha-256' \
    'host matrixmedia mm_app 172.29.0.0/16 scram-sha-256' \
    'host matrixmedia mm_app 172.0.0.0/8 scram-sha-256' \
    'host matrixmedia mm_app 0.0.0.0/0 scram-sha-256' \
    'host matrixmedia mm_admin 172.29.0.0/24 scram-sha-256' \
    'host matrixmedia mm_app 172.29.0.7/32 scram-sha-256' \
    'host matrixmedia all 172.29.0.0/24 scram-sha-256' \
    'host matrixmedia mm_app,mm_media_worker 172.29.0.0/24 scram-sha-256' \
    'host all all 172.28.0.0/14 trust' \
    'hostssl matrixmedia mm_app all scram-sha-256' \
    'host matrixmedia mm_app samenet scram-sha-256' \
    'host matrixmedia mm_app 172.29.0.0 255.255.255.0 scram-sha-256'; do
    out="$(_hba_wider_than_worker <<<"$bad")"
    [ -n "$out" ] || { echo "not flagged: $bad"; return 1; }
  done
  for ok in \
    'host matrixmedia mm_media_worker 172.29.0.0/24 scram-sha-256' \
    'host matrixmedia mm_app 172.28.0.0/24 scram-sha-256' \
    'host matrixmedia mm_app 172.30.0.0/16 scram-sha-256' \
    'host all all 0.0.0.0/0 reject' \
    'host all all ::/0 reject' \
    'local all postgres trust' \
    '# host matrixmedia mm_app 172.16.0.0/12 scram-sha-256'; do
    out="$(_hba_wider_than_worker <<<"$ok")"
    [ -z "$out" ] || { echo "flagged: $ok -> $out"; return 1; }
  done
}

@test "no pg_hba rule lets any other role in from mm-ads-net, however wide" {
  out="$(_hba_wider_than_worker < "$DEPLOY_ROOT/templates/mm-postgres-pg_hba.tmpl.conf")"
  [ -z "$out" ] || { echo "$out"; return 1; }
}

@test "runbook C warns that a firewall reload can drop the hand-inserted rule, and prefers the nft table" {
  rb="$DEPLOY_ROOT/docs/rotation-runbooks.md"
  flat="$(tr '\n' ' ' < "$rb" | sed 's/#  *//g; s/  */ /g')"
  grep -qF '`ufw reload` or `firewall-cmd --reload`' <<<"$flat"          # the step's comment
  grep -qF 'can be dropped by' <<<"$flat"
  grep -qF '`ufw reload` and `firewall-cmd --reload` re-apply the firewall' <<<"$flat"   # the note
  grep -qF 'can drop a rule you inserted into the INPUT chain by hand' <<<"$flat"
  grep -qF 'with nftables (the robust choice)' <<<"$flat"
  grep -qF 'nft list table inet mm_ads' <<<"$flat"
}

@test "mm-postgres joins mm-ads-net and mm-core is not on it" {
  tmpl="$DEPLOY_ROOT/docker-compose.tmpl.yml"
  pg="$(awk '/^  mm-postgres:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$tmpl")"
  grep -q '^      - mm-ads-net$' <<<"$pg"
  core="$(awk '/^  mm-core:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$tmpl")"
  run grep -q 'mm-ads-net' <<<"$core"
  [ "$status" -ne 0 ]
  grep -qE '^        - subnet: 172\.29\.0\.0/24$' "$tmpl"
}

@test "mm-media-worker limits, init and grace period are in the compose block" {
  tmpl="$DEPLOY_ROOT/docker-compose.tmpl.yml"
  block="$(awk '/^  mm-media-worker:/{f=1;print;next} f&&/^  [a-z0-9-]+:/{exit} f' "$tmpl")"
  grep -q '^    init: true$' <<<"$block"
  grep -q '^    stop_grace_period: 30s$' <<<"$block"
  grep -q '^    pids_limit: 64$' <<<"$block"
  grep -q '^    cpus: 1.5$' <<<"$block"
  grep -q '^    cpu_shares: 128$' <<<"$block"
  grep -q '^    mem_limit: 1536m$' <<<"$block"
  grep -qF 'test: ["CMD", "mm-media-worker", "healthcheck"]' <<<"$block"
  # the alive file keeps its default on the tmpfs /tmp (read_only root)
  run grep -E '^\s+MM_MEDIA_WORKER_ALIVE_FILE:' <<<"$block"
  [ "$status" -ne 0 ]
  # the image's VOLUMEs (/data, /etc/matrixmedia) are covered by tiny tmpfs mounts, so the worker
  # has no writable anonymous volume; /tmp holds the alive file only
  grep -qE '^      - /tmp:size=64m,noexec,nosuid,nodev$' <<<"$block"
  grep -qE '^      - /data:size=1m,noexec,nosuid,nodev$' <<<"$block"
  grep -qE '^      - /etc/matrixmedia:size=1m,noexec,nosuid,nodev$' <<<"$block"
  # a core dump would hold the database URL
  grep -qE '^    ulimits:$' <<<"$block"
  grep -qE '^      core: 0$' <<<"$block"
}

# Mutation gaps: whatever widens the sandbox must fail a test, not only ports/labels.
@test "mm-media-worker has no key that widens the sandbox" {
  tmpl="$DEPLOY_ROOT/docker-compose.tmpl.yml"
  block="$(awk '/^  mm-media-worker:/{f=1;print;next} f&&/^  [a-z0-9-]+:/{exit} f' "$tmpl")"
  [ -n "$block" ]
  run grep -E '^\s+(extra_hosts|cap_add|privileged|network_mode|devices|pid|ipc|userns_mode|sysctls|volumes_from|dns|links|external_links):' <<<"$block"
  [ "$status" -ne 0 ] || { echo "$output"; return 1; }
  run grep -E 'unconfined|seccomp:|apparmor:|docker\.sock|/var/run' <<<"$block"
  [ "$status" -ne 0 ] || { echo "$output"; return 1; }
  # the only mounts are the two ad directories
  mounts="$(awk '/^    volumes:/{f=1;next} f&&/^    [a-z_]+:/{exit} f&&/^      - /{print $2}' <<<"$block")"
  [ "$(wc -l <<<"$mounts" | tr -d ' ')" = "2" ] || { echo "$mounts"; return 1; }
  grep -qx '${MM_ROOT:-/opt/mm}/ad-media:/mm-ad-media' <<<"$mounts"
  grep -qx '${MM_ROOT:-/opt/mm}/web/ads:/mm-ads-public' <<<"$mounts"
}

@test "exactly mm-postgres and mm-media-worker are on mm-ads-net, and it keeps a fixed bridge name" {
  tmpl="$DEPLOY_ROOT/docker-compose.tmpl.yml"
  members="$(awk '/^  [a-z0-9-]+:$/{svc=$1} /^      - mm-ads-net$/{print svc}' "$tmpl" | tr -d ':' | sort | tr '\n' ' ')"
  [ "$members" = "mm-media-worker mm-postgres " ] || { echo "members: $members"; return 1; }
  net="$(awk '/^  mm-ads-net:$/{f=1;print;next} f&&/^  [a-z#]/{exit} f' "$tmpl")"
  grep -qE '^    internal: true$' <<<"$net"
  # the host firewall rule in runbook C names this bridge, so the name must not drift
  grep -qE '^      com\.docker\.network\.bridge\.name: mm-ads0$' <<<"$net"
  # Engine 27.x rejects gateway_mode_ipv4=isolated (28.0 added it), so it stays out of the file
  run grep -q 'gateway_mode' <<<"$(grep -vE '^\s*#' <<<"$net")"
  [ "$status" -ne 0 ]
  grep -q 'iptables -I INPUT -i mm-ads0 -m conntrack --ctstate NEW -j DROP' "$DEPLOY_ROOT/docs/rotation-runbooks.md"
  grep -q 'nft add rule inet mm_ads input iifname "mm-ads0" ct state new drop' "$DEPLOY_ROOT/docs/rotation-runbooks.md"
}

@test "mm-core mounts exactly one ad-media directory: incoming, nothing public" {
  tmpl="$DEPLOY_ROOT/docker-compose.tmpl.yml"
  core="$(awk '/^  mm-core:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$tmpl")"
  mounts="$(grep -E '^\s+- .*(ad-media|/web/|/mm-ads)' <<<"$core" || true)"
  [ "$(printf '%s\n' "$mounts" | grep -c .)" = "1" ] || { echo "$mounts"; return 1; }
  grep -qF -- '- ${MM_ROOT:-/opt/mm}/ad-media/incoming:/mm-ad-media/incoming' <<<"$mounts"
  run grep -E '^\s+- .*:/mm-ad-media(:|$)' <<<"$core"      # never the whole media root, never rw sources
  [ "$status" -ne 0 ]
}

# The text of one top-level location block of the nginx template: from its `location` line to the
# first 4-space `}` (a nested location sits on one 8-space line, so it cannot end the block early).
_nginx_loc() {
  awk -v start="$1" '$0 == start {on=1} on{print} on&&/^    \}/{exit}' "$DEPLOY_ROOT/templates/nginx.tmpl.conf"
}

# Ad media is served from the SAME origin as /_mm/chat and /_mm/dashboard, and a compromised media
# worker writes those directories: only .webm/.mp4 may be served, never sniffed, under a CSP that
# grants nothing. add_header in a location REPLACES every inherited one, so each is declared again.
@test "nginx serves /_mm/ads/r/ immutable and /_mm/ads/ for an hour, as media only, never sniffed" {
  conf="$DEPLOY_ROOT/templates/nginx.tmpl.conf"
  r="$(_nginx_loc '    location ^~ /_mm/ads/r/ {')"
  legacy="$(_nginx_loc '    location ^~ /_mm/ads/ {')"
  [ -n "$r" ]
  [ -n "$legacy" ]
  grep -qF 'alias /usr/share/nginx/html/ads/r/;' <<<"$r"
  grep -qF 'alias /usr/share/nginx/html/ads/;' <<<"$legacy"
  for loc in "$r" "$legacy"; do
    grep -qF 'autoindex off;' <<<"$loc"
    # only the two media types, from a two-entry table; the fallback is opaque bytes
    [ "$(grep -c '^        types ' <<<"$loc" | tr -d ' ')" = "1" ]
    grep -qxF '        types { video/webm webm; video/mp4 mp4; }' <<<"$loc"
    grep -qxF '        default_type application/octet-stream;' <<<"$loc"
    run grep -E 'mime\.types|default_type[[:space:]]+(text|image|application/(xhtml|xml|javascript))' <<<"$loc"
    [ "$status" -ne 0 ]
    # nosniff and a CSP that allows only the media itself (no scripts, no other loads), on every
    # response; no `sandbox`: Chromium then refuses to play the file opened directly (preview link)
    grep -qxF '        add_header X-Content-Type-Options "nosniff" always;' <<<"$loc"
    grep -qxF "        add_header Content-Security-Policy \"default-src 'none'; media-src 'self'\" always;" <<<"$loc"
    grep -qxF '        add_header Access-Control-Allow-Origin *;' <<<"$loc"
    # exactly these four add_headers: a fifth would be an unreviewed header, a missing one is
    # inherited-then-lost (server-level headers do not survive a location's own add_header)
    [ "$(grep -c '^        add_header ' <<<"$loc" | tr -d ' ')" = "4" ]
    # everything that is not .webm/.mp4 (directories included) is a 404: the location returns it
    # itself, and a single nested regex location lets exactly the two extensions through
    grep -qxF '        return 404;' <<<"$loc"
    [ "$(grep -c 'location' <<<"$loc" | tr -d ' ')" = "2" ]        # the block itself + one nested
    grep -qxF '        location ~ \.(webm|mp4)$ { }' <<<"$loc"
    # a 404 for a rendition that is not published yet must not be cached: no `always` on Cache-Control
    run grep -E 'Cache-Control.*always' <<<"$loc"
    [ "$status" -ne 0 ]
  done
  # the rendition URLs never change (content-addressed); the legacy {id}.webm keeps its hour
  grep -qxF '        add_header Cache-Control "public, max-age=31536000, immutable";' <<<"$r"
  grep -qxF '        add_header Cache-Control "public, max-age=3600";' <<<"$legacy"
  run grep -F 'immutable' <<<"$legacy"
  [ "$status" -ne 0 ]
  run grep -F 'max-age=3600' <<<"$r"
  [ "$status" -ne 0 ]
  # `^~` on BOTH prefixes: a regex location added later can never take over (the longer /r/ prefix
  # still wins for its paths). Nothing else matches /_mm/ads, and only these two top-level locations.
  [ "$(grep -cE '^    location (\^~ |= |~\*? )?/_mm/ads/' "$conf")" = "2" ]
  [ "$(grep -cE '^    location \^~ /_mm/ads/(r/)? \{$' "$conf")" = "2" ]
  run grep -E '^    location[[:space:]]+~' "$conf"       # no top-level regex location at all
  [ "$status" -ne 0 ]
  run grep -E '^\s*location\s+~' "$conf"
  [ "$(grep -c '_mm/ads' <<<"$output" | tr -d ' ')" = "0" ]
}

# Traefik v3's default readTimeout is 60 s and covers the whole request body, so without this a
# 100 MB ad upload over a slow uplink is cut mid-stream. The intake bounds slow uploads itself
# (idle timeout, overall deadline), so Traefik must not cut first.
@test "traefik's websecure entrypoint lets an ad upload run for at least 30 minutes" {
  tmpl="$DEPLOY_ROOT/docker-compose.tmpl.yml"
  tf="$(awk '/^  traefik:$/{on=1; print; next} on && /^  [a-zA-Z#]/{exit} on' "$tmpl")"
  [ -n "$tf" ]
  line="$(grep -E '^      - --entrypoints\.websecure\.transport\.respondingtimeouts\.readtimeout=' <<<"$tf")"
  [ "$(grep -c . <<<"$line" | tr -d ' ')" = "1" ]
  val="${line##*=}"
  case "$val" in
    *h) secs=$(( ${val%h} * 3600 )) ;;
    *m) secs=$(( ${val%m} * 60 )) ;;
    *s) secs=${val%s} ;;
    *)  echo "readtimeout needs a unit: $val"; return 1 ;;
  esac
  [ "$secs" -ge 1800 ]
  # the plain-http entrypoint only redirects: the timeout belongs on the one that carries uploads
  run grep -E 'entrypoints\.web\.transport\.respondingtimeouts' <<<"$tf"
  [ "$status" -ne 0 ]
}

@test "the installed compose file is checked for the mm-core ad upload mount" {
  printf 'services:\n  mm-core:\n    volumes:\n      - mm-data:/data\n      - /opt/mm/web/ads:/opt/MatrixMedia/web/ads\n' > "$MM_ROOT/docker-compose.yml"
  run compose_mounts_ad_incoming
  [ "$status" -ne 0 ]
  printf '      # - /x/ad-media/incoming:/mm-ad-media/incoming (a comment)\n' >> "$MM_ROOT/docker-compose.yml"
  run compose_mounts_ad_incoming
  [ "$status" -ne 0 ]
  printf '      - ${MM_ROOT:-/opt/mm}/ad-media/incoming:/mm-ad-media/incoming\n' >> "$MM_ROOT/docker-compose.yml"
  run compose_mounts_ad_incoming
  [ "$status" -eq 0 ]
  rm "$MM_ROOT/docker-compose.yml"
  run compose_mounts_ad_incoming
  [ "$status" -ne 0 ]
  # and the template itself passes
  cp "$DEPLOY_ROOT/docker-compose.tmpl.yml" "$MM_ROOT/docker-compose.yml"
  run compose_mounts_ad_incoming
  [ "$status" -eq 0 ]
}

@test "mm_media_worker_role.sql caps the worker's sessions at 12 and stays re-runnable" {
  sql="$DEPLOY_ROOT/sql/mm_media_worker_role.sql"
  grep -qx 'ALTER ROLE mm_media_worker CONNECTION LIMIT 12;' "$sql"
  head -3 "$sql" | grep -q 'Idempotent'
}

@test "the image pins the matrixmedia ids that install.sh chowns the ad directories to" {
  df="$DEPLOY_ROOT/../infra/docker/Dockerfile"
  grep -q 'addgroup --system --gid 101 matrixmedia' "$df"
  grep -q 'adduser --system --uid 100 --ingroup matrixmedia --no-create-home matrixmedia' "$df"
  grep -qE 'install -d -m 0750 -o 100 -g 101 ' "$DEPLOY_ROOT/install.sh"
}

@test "install.sh creates the ad media directories for the image's matrixmedia uid, private ones 0750" {
  i="$DEPLOY_ROOT/install.sh"
  grep -qE 'install -d -m 0750 -o 100 -g 101 "\$MM_ROOT/ad-media" "\$MM_ROOT/ad-media/incoming"' "$i"
  grep -q '"\$MM_ROOT/ad-media/sources" "\$MM_ROOT/ad-media/work"' "$i"
  grep -qE 'install -d -m 0755 -o 100 -g 101 "\$MM_ROOT/web/ads" "\$MM_ROOT/web/ads/r"' "$i"
}

@test "the mm-core image ships mm-media-worker, and the media-worker target adds ffmpeg and keeps the uid" {
  df="$DEPLOY_ROOT/../infra/docker/Dockerfile"
  grep -q 'cargo build --release -p mm-server -p mm-fleet-runner -p mm-media-worker ' "$df"
  grep -q '^COPY --from=builder /usr/local/bin/mm-media-worker /usr/local/bin/mm-media-worker$' "$df"
  # one source of truth: the worker is a target of the mm-core Dockerfile, not a second file
  [ ! -e "$DEPLOY_ROOT/../infra/docker/Dockerfile.media-worker" ]
  stage=$(awk '/^FROM runtime AS media-worker$/{f=1; next} f && /^FROM /{exit} f' "$df")
  [ -n "$stage" ]
  grep -qE 'apt-get install -y --no-install-recommends ffmpeg( |$)' <<<"$stage"
  grep -q '^USER matrixmedia$' <<<"$stage"
  grep -q '^ENTRYPOINT \["mm-media-worker"\]$' <<<"$stage"
  # a plain build (no --target) must still be mm-core: the last stage is the runtime, unchanged
  [ "$(grep '^FROM ' "$df" | tail -1)" = "FROM runtime AS mm-core" ]
  [ -z "$(awk '/^FROM runtime AS mm-core$/{f=1; next} f && NF && !/^#/' "$df")" ]
}

@test "CI builds and publishes the mm-media-worker image from the Dockerfile's media-worker target" {
  wf="$DEPLOY_ROOT/../.github/workflows/build-images.yml"
  entry() { awk -v img="$1" '$0 ~ "- image: " img "$" {f=1} f {print} f && /target:/ {exit}' "$wf"; }
  w=$(entry mm-media-worker)
  grep -q 'dockerfile: infra/docker/Dockerfile$' <<<"$w"
  grep -q 'context: \.$' <<<"$w"
  grep -q 'target: media-worker$' <<<"$w"
  # mm-core pins its own target, so a stage added later can never change what it builds
  entry mm-core | grep -q 'target: mm-core$'
  grep -q 'target: \${{ matrix.target }}$' "$wf"
  # the name CI publishes (matrixmedia-<image>) is the one the compose template pulls
  grep -q 'images: .*/matrixmedia-\${{ matrix.image }}$' "$wf"
  grep -q 'image: \${MM_REGISTRY}/matrixmedia-mm-media-worker:\${MM_VERSION}$' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}
