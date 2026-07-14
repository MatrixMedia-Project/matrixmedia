# shellcheck shell=bash
: "${MM_ROOT:=/opt/mm}"

# backup_configs -- tar the config dir + env files (only those that exist).
backup_configs() {
  mkdir -p "$MM_ROOT/backups"
  local ts; ts="$(date -u +%Y%m%d-%H%M%S)"
  local items=() f
  for f in config .env .env.secrets; do [ -e "$MM_ROOT/$f" ] && items+=("$f"); done
  tar czf "$MM_ROOT/backups/config-$ts.tar.gz" -C "$MM_ROOT" "${items[@]}"
  log "config backup: $MM_ROOT/backups/config-$ts.tar.gz"
}

# dump_one CONTAINER OUT -- pg_dumpall CONTAINER into OUT.gz. Non-zero on ANY failure.
#
# This used to be `docker exec ... | gzip > out || warn`. Two bugs, compounding:
#
#   1. A pipeline's exit status is the LAST command's — gzip's — and gzip happily succeeds
#      compressing nothing. So `|| warn` never fired, even when pg_dumpall failed outright.
#   2. The result was a ~20-byte gz that EXISTS and is FRESH. Every "do we have a backup?"
#      check therefore passed, while the file contained no database at all.
#
# Which means `mmctl upgrade` would take its "backup", see a fresh dump on disk, and
# proceed — having destroyed the operator's rollback while telling them they had one. The
# failure only surfaces at restore, i.e. at the worst possible moment.
dump_one() {
  local container="$1" out="$2" sz
  # Subshell so pipefail does not leak into the caller's shell options.
  if ! ( set -o pipefail; docker exec "$container" pg_dumpall -U postgres | gzip > "$out" ); then
    rm -f "$out"          # leave no phantom that a freshness check would trust
    warn "pg dump FAILED for $container"
    return 1
  fi
  sz="$(wc -c < "$out" | tr -d ' ')"
  # gzip of an empty stream is ~20 bytes. A real dump — even of an empty cluster — carries
  # roles, encodings and DDL, and is far larger.
  if [ "${sz:-0}" -lt 200 ]; then
    rm -f "$out"
    warn "pg dump for $container produced only ${sz} bytes — that is not a backup"
    return 1
  fi
}

# backup_db -- pg_dumpall both Postgres instances. Non-zero if EITHER fails.
backup_db() {
  mkdir -p "$MM_ROOT/backups"
  local ts; ts="$(date -u +%Y%m%d-%H%M%S)" rc=0
  dump_one matrixmedia-postgres-1    "$MM_ROOT/backups/pg-synapse-$ts.sql.gz" || rc=1
  dump_one matrixmedia-mm-postgres-1 "$MM_ROOT/backups/pg-app-$ts.sql.gz"     || rc=1
  return "$rc"
}

# backup -- config + both databases. Non-zero if ANY part failed.
#
# The return value is load-bearing: `mmctl upgrade` refuses to proceed when it is non-zero,
# because the backup IS the rollback (the DB rolls forward only).
backup() {
  local rc=0
  backup_configs || rc=1
  backup_db      || rc=1
  return "$rc"
}
