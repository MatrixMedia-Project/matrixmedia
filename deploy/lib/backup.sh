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

# backup_db -- pg_dumpall both Postgres instances via the running containers.
backup_db() {
  mkdir -p "$MM_ROOT/backups"
  local ts; ts="$(date -u +%Y%m%d-%H%M%S)"
  docker exec matrixmedia-postgres-1   pg_dumpall -U postgres | gzip > "$MM_ROOT/backups/pg-synapse-$ts.sql.gz" || warn "synapse pg dump failed"
  docker exec matrixmedia-mm-postgres-1 pg_dumpall -U postgres | gzip > "$MM_ROOT/backups/pg-app-$ts.sql.gz" || warn "app pg dump failed"
}

backup() { backup_configs; backup_db; }
