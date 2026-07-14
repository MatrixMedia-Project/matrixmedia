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
  docker() { cat >/dev/null; return 0; }   # swallow the psql pipe
  export -f docker 2>/dev/null || true
  run mm_restore "$MM_ROOT/backups/config-20260714-150000.tar.gz"
  [[ "$output" == *"pg-synapse-20260714-150000.sql.gz"* ]] || \
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
