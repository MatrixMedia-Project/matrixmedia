#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# MatrixMedia PostgreSQL Backup Script
#
# Creates compressed pg_dump backups with integrity verification and retention.
#
# Configuration (environment variables):
#   PG_HOST           PostgreSQL host            (default: localhost)
#   PG_PORT           PostgreSQL port            (default: 5432)
#   PG_USER           PostgreSQL user            (default: matrixmedia)
#   PG_DB             PostgreSQL database        (default: matrixmedia)
#   BACKUP_DIR        Directory for backups      (default: ./backups)
#   RETENTION_DAYS    Delete backups older than N (default: 30)
#   PGPASSWORD        PostgreSQL password         (standard libpq var)
#   DOCKER_CONTAINER  If set, exec pg_dump inside this container instead
#                     of using a local pg_dump binary.
#
# Usage:
#   bash scripts/pg-backup.sh
#   PG_HOST=db.prod.internal BACKUP_DIR=/mnt/backups bash scripts/pg-backup.sh
#   DOCKER_CONTAINER=matrixmedia-postgres-1 bash scripts/pg-backup.sh
#
# Exit codes:
#   0  Success
#   1  Failure (dump failed, verification failed, etc.)
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
BACKUP_DIR="${BACKUP_DIR:-./backups}"
RETENTION_DAYS="${RETENTION_DAYS:-30}"
DOCKER_CONTAINER="${DOCKER_CONTAINER:-}"

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------
if [[ -z "${DOCKER_CONTAINER}" ]]; then
  command -v pg_dump >/dev/null 2>&1 || die "pg_dump not found. Install postgresql-client or set DOCKER_CONTAINER."
fi
command -v gzip >/dev/null 2>&1 || die "gzip not found."

mkdir -p "$BACKUP_DIR" || die "Cannot create backup directory: $BACKUP_DIR"

# ---------------------------------------------------------------------------
# Dump
# ---------------------------------------------------------------------------
TS=$(date +%Y-%m-%d_%H%M%S)
BACKUP_FILE="${BACKUP_DIR}/matrixmedia_${TS}.sql.gz"

log "Starting backup of ${PG_DB}@${PG_HOST}:${PG_PORT} as ${PG_USER}"

if [[ -n "${DOCKER_CONTAINER}" ]]; then
  log "Using Docker container: ${DOCKER_CONTAINER}"
  docker exec -e PGPASSWORD="${PGPASSWORD:-}" "${DOCKER_CONTAINER}" \
    pg_dump -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$PG_DB" \
    | gzip > "$BACKUP_FILE" \
    || die "pg_dump via Docker failed"
else
  pg_dump -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$PG_DB" \
    | gzip > "$BACKUP_FILE" \
    || die "pg_dump failed"
fi

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

# Check the file exists and is non-empty
[[ -f "$BACKUP_FILE" ]] || die "Backup file not created: $BACKUP_FILE"

# Cross-platform file size
SIZE=$(stat -f%z "$BACKUP_FILE" 2>/dev/null || stat -c%s "$BACKUP_FILE" 2>/dev/null || echo 0)
[[ "$SIZE" -gt 0 ]] || die "Backup file is empty (0 bytes): $BACKUP_FILE"

# Verify gzip integrity
gzip -t "$BACKUP_FILE" || die "Backup file failed gzip integrity check: $BACKUP_FILE"

log "Backup complete: ${BACKUP_FILE} (${SIZE} bytes)"

# ---------------------------------------------------------------------------
# Retention
# ---------------------------------------------------------------------------
if [[ "$RETENTION_DAYS" -gt 0 ]]; then
  DELETED=$(find "$BACKUP_DIR" -name "matrixmedia_*.sql.gz" -mtime +"$RETENTION_DAYS" -print -delete 2>/dev/null | wc -l | tr -d ' ')
  log "Retention: deleted ${DELETED} backup(s) older than ${RETENTION_DAYS} days"
fi

log "Done."
exit 0
