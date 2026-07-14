load helper

setup() {
  setup_tmp
  source "$DEPLOY_ROOT/lib/common.sh"
  source "$DEPLOY_ROOT/lib/backup.sh"
  source "$DEPLOY_ROOT/lib/lifecycle.sh"
  mkdir -p "$MM_ROOT/backups"
  DC=(true)   # stub: no docker in unit tests
}
teardown() { teardown_tmp; }

_mk_backup() {   # _mk_backup TS [AGE_SECS]
  local ts="$1" age="${2:-0}"
  : > "$MM_ROOT/backups/config-$ts.tar.gz"
  : > "$MM_ROOT/backups/pg-app-$ts.sql.gz"
  : > "$MM_ROOT/backups/pg-synapse-$ts.sql.gz"
  if [ "$age" -gt 0 ]; then
    local when; when=$(( $(date -u +%s) - age ))
    touch -t "$(date -u -r "$when" +%Y%m%d%H%M.%S 2>/dev/null || date -u -d "@$when" +%Y%m%d%H%M.%S)" \
      "$MM_ROOT/backups/config-$ts.tar.gz"
  fi
}

# ── the rollback contract ────────────────────────────────────────────────────

@test "backup_is_fresh accepts a new backup and rejects a stale one" {
  # An upgrade that 'took a backup' by finding last month's is not a rollback plan.
  _mk_backup 20260714-000000 0
  run backup_is_fresh "$MM_ROOT/backups/config-20260714-000000.tar.gz" 3600
  [ "$status" -eq 0 ]

  _mk_backup 20260601-000000 999999
  run backup_is_fresh "$MM_ROOT/backups/config-20260601-000000.tar.gz" 3600
  [ "$status" -ne 0 ]
}

@test "backup_is_fresh rejects a backup that does not exist" {
  run backup_is_fresh "$MM_ROOT/backups/nope.tar.gz" 3600; [ "$status" -ne 0 ]
  run backup_is_fresh "" 3600;                            [ "$status" -ne 0 ]
}

@test "upgrade REFUSES when the backup fails" {
  # The whole contract. The DB rolls forward only, so restore-from-backup is the ONLY way
  # out of a bad upgrade. Upgrading without one silently removes the operator's only exit —
  # so a failed backup must abort, not warn.
  backup() { return 1; }          # simulate a failing backup
  run mm_upgrade
  [ "$status" -ne 0 ]
  [[ "$output" == *"BACKUP FAILED"* ]]
  [[ "$output" == *"refusing to upgrade"* ]]
}

@test "upgrade REFUSES when backup 'succeeds' but leaves nothing fresh on disk" {
  # A backup command that exits 0 without writing anything would otherwise sail through.
  backup() { return 0; }          # exits clean, writes nothing
  run mm_upgrade
  [ "$status" -ne 0 ]
  [[ "$output" == *"refusing to upgrade"* ]]
}

@test "upgrade names the rollback point it just created" {
  backup() { _mk_backup 20260714-120000 0; }
  self_smoke() { return 0; }
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  printf 'MM_ADMIN_TOKEN=t\n'      > "$MM_ROOT/.env.secrets"

  run mm_upgrade
  [ "$status" -eq 0 ]
  # The operator must be told WHERE their exit is, by path, before anything changes.
  [[ "$output" == *"rollback point is"* ]]
  [[ "$output" == *"config-20260714-120000.tar.gz"* ]]
}

@test "a failed post-upgrade smoke tells the operator the DB cannot roll back" {
  backup() { _mk_backup 20260714-130000 0; }
  self_smoke() { return 1; }
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  printf 'MM_ADMIN_TOKEN=t\n'      > "$MM_ROOT/.env.secrets"

  run mm_upgrade
  [ "$status" -ne 0 ]
  # Silence here would leave an operator re-running `docker compose up` with an old image
  # against a migrated schema, which is undefined behaviour, not a rollback.
  [[ "$output" == *"CANNOT be migrated back"* ]]
  [[ "$output" == *"mmctl restore"* ]]
}

# ── consent on destructive verbs ─────────────────────────────────────────────

@test "uninstall WITHOUT --purge keeps the data volumes" {
  run mm_uninstall
  [ "$status" -eq 0 ]
  [[ "$output" == *"DATA VOLUMES KEPT"* ]]
  [[ "$output" != *"volumes destroyed"* ]]
}

@test "uninstall --purge aborts unless the operator types yes" {
  # 'uninstall' does not tell anyone their messages are about to be deleted. --purge must
  # not act on a default.
  run bash -c "echo no | { $(declare -f mm_uninstall confirm latest_backup log warn die); DC=(true); MM_ROOT='$MM_ROOT'; mm_uninstall --purge; }"
  [ "$status" -ne 0 ]
  [[ "$output" == *"aborted"* ]]
}

@test "restore aborts unless the operator types yes" {
  _mk_backup 20260714-140000 0
  run bash -c "echo no | { $(declare -f mm_restore confirm latest_backup log warn die); DC=(true); MM_ROOT='$MM_ROOT'; mm_restore; }"
  [ "$status" -ne 0 ]
  [[ "$output" == *"aborted"* ]]
}

@test "restore refuses when there is no backup at all" {
  run mm_restore
  [ "$status" -ne 0 ]
  [[ "$output" == *"no backup to restore"* ]]
}

@test "restore pairs the config tarball with the DB dumps of the SAME timestamp" {
  # Restoring config from one point and a database from another produces a server whose
  # config and data disagree — worse than either alone.
  _mk_backup 20260714-150000 0
  MM_ASSUME_YES=1
  # `docker() { cat >/dev/null; }` would BLOCK: restore_one issues `docker exec ... -c SQL`
  # with no stdin, so cat waits on the terminal forever.
  docker() { return 0; }
  run mm_restore "$MM_ROOT/backups/config-20260714-150000.tar.gz"
  [[ "$output" == *"20260714-150000"* ]]
}

@test "secrets NEVER prints a secret value" {
  # This runs on a terminal an operator may be screen-sharing.
  printf 'MM_ADMIN_TOKEN=super-secret-value\nLK_API_SECRET=another-one\n' > "$MM_ROOT/.env.secrets"
  run mm_secrets
  [ "$status" -eq 0 ]
  [[ "$output" == *"MM_ADMIN_TOKEN"* ]]
  [[ "$output" != *"super-secret-value"* ]]
  [[ "$output" != *"another-one"* ]]
}

@test "mmctl advertises every lifecycle verb" {
  run bash "$DEPLOY_ROOT/mmctl"
  [ "$status" -ne 0 ]
  for verb in check upgrade restore uninstall secrets init; do
    [[ "$output" == *"$verb"* ]] || { echo "usage does not mention: $verb"; false; }
  done
}

@test "upgrade REFUSES a config-only backup with no database dump" {
  # The hole this closes: backup_configs can succeed while both pg dumps fail. A config
  # tarball is not a rollback — the thing you cannot rebuild is the DATABASE. Upgrading on
  # a config-only backup destroys the operator's rollback while reporting one.
  backup() { : > "$MM_ROOT/backups/config-20260714-160000.tar.gz"; return 0; }
  run mm_upgrade
  [ "$status" -ne 0 ]
  [[ "$output" == *"database dump"* ]]
  [[ "$output" == *"a config backup is not a rollback"* ]]
}

@test "backup() returns NON-ZERO when a database dump fails, and leaves no phantom file" {
  # It used to return 0 and write a ~20-byte gz: `docker exec | gzip > f` reports GZIP's
  # status, which succeeds compressing nothing. So `|| warn` never fired, the file existed,
  # was fresh, and every "do we have a backup?" check passed — on a file containing no
  # database. The failure would surface at restore, i.e. at the worst possible moment.
  mkdir -p "$MM_ROOT/config"; echo x > "$MM_ROOT/config/a"
  docker() { return 1; }
  run backup
  [ "$status" -ne 0 ]
  [ "$(ls "$MM_ROOT/backups/" 2>/dev/null | grep -c 'sql.gz' || true)" -eq 0 ]
}

@test "the two Postgres clusters use their OWN superuser (synapse is NOT 'postgres')" {
  # The Synapse cluster's POSTGRES_USER is `synapse`. backup/restore used `-U postgres` for
  # BOTH, so pg_dump against Synapse failed with `role "postgres" does not exist` — meaning
  # the Synapse database (every message, every account) had NEVER been backed up and could
  # never be restored.
  grep -q 'MM_PG_SYNAPSE_SUPERUSER="synapse"' "$DEPLOY_ROOT/lib/backup.sh"
  grep -q 'MM_PG_APP_SUPERUSER="postgres"'    "$DEPLOY_ROOT/lib/backup.sh"

  # And the compose file must still agree — if someone changes POSTGRES_USER, this breaks.
  grep -qE '^\s+POSTGRES_USER: synapse$'  "$DEPLOY_ROOT/docker-compose.tmpl.yml"
  grep -qE '^\s+POSTGRES_USER: postgres$' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

@test "restore uses ON_ERROR_STOP (without it psql exits 0 having applied nothing)" {
  # THE false success. psql without ON_ERROR_STOP continues past every error and exits 0, so
  # a restore that changed nothing reported success — at exactly the moment an operator was
  # relying on it to roll back a bad upgrade.
  grep -q 'ON_ERROR_STOP=1' "$DEPLOY_ROOT/lib/lifecycle.sh"
  # And it must DROP + CREATE, not replay into the existing database.
  grep -q 'DROP DATABASE IF EXISTS' "$DEPLOY_ROOT/lib/lifecycle.sh"
  grep -q 'CREATE DATABASE'         "$DEPLOY_ROOT/lib/lifecycle.sh"
}

@test "mm_check treats an EXITED container as NOT running" {
  # `exited` is a crashed container. The filter excluded both `running` AND `exited`, so a
  # dead service never showed up in "services not running" — the one question check answers.
  ! grep -q "grep -vE ' (running|exited)\$'" "$DEPLOY_ROOT/lib/lifecycle.sh"
}
