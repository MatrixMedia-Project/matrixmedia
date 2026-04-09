#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# MatrixMedia PostgreSQL Restore Script
#
# Restores a compressed pg_dump backup into the target database.
#
# Configuration (environment variables):
#   PG_HOST           PostgreSQL host            (default: localhost)
#   PG_PORT           PostgreSQL port            (default: 5432)
#   PG_USER           PostgreSQL user            (default: matrixmedia)
#   PG_DB             PostgreSQL database        (default: matrixmedia)
#   PGPASSWORD        PostgreSQL password         (standard libpq var)
#   DOCKER_CONTAINER  If set, pipe restore through this container's psql.
#
# Usage:
#   bash scripts/pg-restore.sh backups/matrixmedia_2026-04-09_120000.sql.gz
#   DOCKER_CONTAINER=matrixmedia-postgres-1 bash scripts/pg-restore.sh backups/matrixmedia_2026-04-09_120000.sql.gz
#
# WARNING: This will overwrite existing data in the target database.
#          Stop mm-core before restoring to avoid conflicts.
#
# Exit codes:
#   0  Success
#   1  Failure
# =============================================================================

log() {
  echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"
}

die() {
  log "ERROR: $*" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-matrixmedia}"
PG_DB="${PG_DB:-matrixmedia}"
DOCKER_CONTAINER="${DOCKER_CONTAINER:-}"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <backup-file.sql.gz>"
  echo ""
  echo "Example:"
  echo "  bash scripts/pg-restore.sh backups/matrixmedia_2026-04-09_120000.sql.gz"
  exit 1
fi

BACKUP_FILE="$1"

[[ -f "$BACKUP_FILE" ]] || die "Backup file not found: $BACKUP_FILE"

# ---------------------------------------------------------------------------
# Verify backup before restoring
# ---------------------------------------------------------------------------
log "Verifying backup integrity: $BACKUP_FILE"
gzip -t "$BACKUP_FILE" || die "Backup file failed gzip integrity check"

SIZE=$(stat -f%z "$BACKUP_FILE" 2>/dev/null || stat -c%s "$BACKUP_FILE" 2>/dev/null || echo 0)
[[ "$SIZE" -gt 0 ]] || die "Backup file is empty"

log "Backup file OK (${SIZE} bytes)"

# ---------------------------------------------------------------------------
# Confirm
# ---------------------------------------------------------------------------
log "WARNING: This will restore into ${PG_DB}@${PG_HOST}:${PG_PORT} as ${PG_USER}"
log "WARNING: Existing data may be overwritten. Stop mm-core first."

if [[ -t 0 ]]; then
  read -rp "Continue? [y/N] " CONFIRM
  [[ "$CONFIRM" =~ ^[Yy]$ ]] || { log "Aborted."; exit 0; }
fi

# ---------------------------------------------------------------------------
# Restore
# ---------------------------------------------------------------------------
log "Restoring from ${BACKUP_FILE}..."

if [[ -n "${DOCKER_CONTAINER}" ]]; then
  log "Using Docker container: ${DOCKER_CONTAINER}"
  gunzip -c "$BACKUP_FILE" \
    | docker exec -i -e PGPASSWORD="${PGPASSWORD:-}" "${DOCKER_CONTAINER}" \
      psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$PG_DB" \
    || die "Restore via Docker failed"
else
  gunzip -c "$BACKUP_FILE" \
    | psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$PG_DB" \
    || die "Restore failed"
fi

log "Restore complete."
exit 0
