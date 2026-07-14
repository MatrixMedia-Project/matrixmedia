# shellcheck shell=bash
: "${MM_ROOT:=/opt/mm}"

# The two Postgres clusters do NOT share a superuser.
#
#   postgres      (Synapse)   POSTGRES_USER: synapse    <- NOT "postgres"
#   mm-postgres   (app)       POSTGRES_USER: postgres
#
# backup and restore used `-U postgres` for BOTH. Against the Synapse cluster that role
# does not exist, so pg_dumpall failed with `FATAL: role "postgres" does not exist` — which
# means the SYNAPSE DATABASE — every message, every account — HAS NEVER BEEN BACKED UP, and
# `mmctl restore` could never have restored it. These two facts are kept together, here, so
# that the next person cannot get them apart.
# Each cluster: container | superuser | the database that actually holds the data | its owner.
#
#   synapse cluster   postgres      superuser synapse    db synapse      owner synapse
#   app cluster       mm-postgres   superuser postgres   db matrixmedia  owner mm_admin
#
# (`matrixmedia` is created by mm-postgres-init.tmpl.sql; the app cluster's POSTGRES_DB is
# just the default `postgres` maintenance database and holds nothing.)
# shellcheck disable=SC2034  # consumed by lifecycle.sh (restore), which shellcheck lints separately
MM_PG_SYNAPSE_CONTAINER="matrixmedia-postgres-1"
MM_PG_SYNAPSE_SUPERUSER="synapse"
MM_PG_SYNAPSE_DB="synapse"
MM_PG_SYNAPSE_OWNER="synapse"

MM_PG_APP_CONTAINER="matrixmedia-mm-postgres-1"
MM_PG_APP_SUPERUSER="postgres"
MM_PG_APP_DB="matrixmedia"
MM_PG_APP_OWNER="mm_admin"

# backup_configs TS -- tar the config dir + env files (only those that exist).
backup_configs() {
  local ts="$1"
  mkdir -p "$MM_ROOT/backups"
  local items=() f out="$MM_ROOT/backups/config-$ts.tar.gz"
  for f in config .env .env.secrets; do [ -e "$MM_ROOT/$f" ] && items+=("$f"); done

  # Check tar. This used to be a bare `tar czf ...` followed by `log ...`, and because the
  # function is invoked as `backup_configs || rc=1` (which disables errexit inside it), a
  # failing tar fell through to `log`, whose exit 0 became the return value. On a full disk
  # that leaves a TRUNCATED tarball with a current mtime — which every freshness check then
  # trusts as a rollback point.
  if ! tar czf "$out" -C "$MM_ROOT" "${items[@]}"; then
    rm -f "$out"        # leave no partial file for a freshness check to believe in
    warn "config backup FAILED (tar)"
    return 1
  fi
  log "config backup: $out"
}

# dump_one CONTAINER SUPERUSER DB OUT -- pg_dump one DATABASE into OUT.gz. Non-zero on ANY failure.
#
# pg_dump of a single database, NOT pg_dumpall of the cluster. pg_dumpall dumps roles and
# every database, and a `pg_dumpall --clean` stream cannot restore itself: its DROP DATABASE
# cannot drop the database psql is connected to, and its DROP ROLE cannot drop the role psql
# is connected as. Restoring one database into a freshly-created empty one has neither
# problem. Roles are created by the installer, not by the backup.
#
# The old code also piped through `|| warn`, which never fired: a pipeline reports the LAST
# command's status — gzip's — and gzip succeeds compressing nothing. So a failed dump left a
# ~20-byte gz that EXISTS and is FRESH, and every "do we have a backup?" check believed it.
dump_one() {
  local container="$1" superuser="$2" db="$3" out="$4" sz
  # Subshell so pipefail does not leak into the caller's shell options.
  if ! ( set -o pipefail
         docker exec "$container" pg_dump -U "$superuser" -d "$db" | gzip > "$out" ); then
    rm -f "$out"          # leave no phantom that a freshness check would trust
    warn "pg_dump FAILED for $db on $container (superuser: $superuser)"
    return 1
  fi
  sz="$(wc -c < "$out" | tr -d ' ')"
  # gzip of an empty stream is ~20 bytes. A real dump carries DDL even for an empty database.
  if [ "${sz:-0}" -lt 200 ]; then
    rm -f "$out"
    warn "pg_dump for $db produced only ${sz} bytes — that is not a backup"
    return 1
  fi
}

# backup_db TS -- dump both databases. Non-zero if EITHER fails.
backup_db() {
  # `local ts rc=0`, NOT `local ts; ts=... rc=0`. In the latter only `ts` is local, so
  # bash's dynamic scoping writes `rc` straight into the CALLER's `rc` — resetting to 0 a
  # failure the caller had already recorded. That is exactly how a failed config backup
  # turned into a successful `backup()`.
  local ts="$1" rc=0
  mkdir -p "$MM_ROOT/backups"
  dump_one "$MM_PG_SYNAPSE_CONTAINER" "$MM_PG_SYNAPSE_SUPERUSER" "$MM_PG_SYNAPSE_DB" \
           "$MM_ROOT/backups/pg-synapse-$ts.sql.gz" || rc=1
  dump_one "$MM_PG_APP_CONTAINER" "$MM_PG_APP_SUPERUSER" "$MM_PG_APP_DB" \
           "$MM_ROOT/backups/pg-app-$ts.sql.gz" || rc=1
  return "$rc"
}

# backup -- config + both databases, under ONE timestamp. Non-zero if ANY part failed.
#
# The timestamp is taken once and passed down. It used to be generated independently inside
# backup_configs and backup_db, so a tar that happened to cross a second boundary — routine
# — left the DB dumps under a different suffix than the config tarball. Everything
# downstream (upgrade's rollback check, restore's pairing) finds the dumps BY the config
# tarball's timestamp, and would simply not find them.
#
# The return value is load-bearing: `mmctl upgrade` refuses to proceed when it is non-zero,
# because the backup IS the rollback (the DB rolls forward only).
backup() {
  local rc=0 ts
  ts="$(date -u +%Y%m%d-%H%M%S)"
  backup_configs "$ts" || rc=1
  backup_db "$ts"      || rc=1
  return "$rc"
}
