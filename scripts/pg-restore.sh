#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# MatrixMedia PostgreSQL Restore
#
# Restores the GPG-encrypted, custom-format pg_dump backups produced by
# mm-postgres-backup.sh and synapse-postgres-backup.sh (files named *.sql.gpg).
# Those are `pg_dump --format=custom | gpg --symmetric`, so restore is
# `gpg --decrypt | pg_restore` (NOT gunzip|psql — the old version assumed plain
# gzip'd SQL and could not read the real backups).
#
# Environment variables:
#   PG_USER             PostgreSQL user            (default: mm_admin)
#   PG_DB               target database            (default: matrixmedia)
#   DOCKER_CONTAINER    run pg_restore inside this container (recommended on prod;
#                       e.g. matrixmedia-mm-postgres-1 or matrixmedia-postgres-1)
#   PG_HOST / PG_PORT   used only when DOCKER_CONTAINER is unset (default localhost:5432)
#   GPG_PASSPHRASE      symmetric passphrase (overrides the file)
#   GPG_PASSPHRASE_FILE file holding the passphrase
#                       (default: /opt/MatrixMedia/secrets/mm_db_backup_passphrase)
#   LIST_ONLY=1         decrypt + `pg_restore --list` only — verifies the archive
#                       is intact and restorable WITHOUT writing any data. Safe to
#                       run anytime; use it for backup-integrity / DR drills.
#   CLEAN=1             pass --clean --if-exists (drop existing objects first)
#
# Usage:
#   # Integrity / DR drill (no writes):
#   LIST_ONLY=1 DOCKER_CONTAINER=matrixmedia-mm-postgres-1 PG_USER=mm_admin \
#     bash scripts/pg-restore.sh /opt/MatrixMedia/backups/mm-postgres/matrixmedia_*.sql.gpg
#
#   # Real restore into a throwaway DB (drill that loads data):
#   DOCKER_CONTAINER=matrixmedia-mm-postgres-1 PG_DB=mm_restore_drill PG_USER=mm_admin \
#     bash scripts/pg-restore.sh <file.sql.gpg>
#
#   # Recovery into the live DB (stop the consumer first!):
#   CLEAN=1 DOCKER_CONTAINER=matrixmedia-mm-postgres-1 PG_DB=matrixmedia PG_USER=mm_admin \
#     bash scripts/pg-restore.sh <file.sql.gpg>
#
# WARNING: a real restore overwrites the target database. Restore into a
#          throwaway PG_DB to drill; stop mm-core / Synapse before restoring live.
# =============================================================================

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }
die() { log "ERROR: $*" >&2; exit 1; }

PG_HOST="${PG_HOST:-127.0.0.1}"   # TCP host so pg_hba scram rules match (mm-postgres rejects the local socket)
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-mm_admin}"
PG_DB="${PG_DB:-matrixmedia}"
DOCKER_CONTAINER="${DOCKER_CONTAINER:-}"
# Sidecar mode: mm-postgres's pg_hba only accepts connections from its docker
# network (not the local socket or 127.0.0.1), so a real restore must run a
# client container ON that network and reach the DB by its container hostname —
# exactly how mm-postgres-backup.sh dumps. Set DOCKER_NETWORK + PG_HOST=<container>.
DOCKER_NETWORK="${DOCKER_NETWORK:-}"
PG_IMAGE="${PG_IMAGE:-postgres:16-alpine}"
GPG_PASSPHRASE_FILE="${GPG_PASSPHRASE_FILE:-/opt/MatrixMedia/secrets/mm_db_backup_passphrase}"
LIST_ONLY="${LIST_ONLY:-}"
CLEAN="${CLEAN:-}"

[[ $# -ge 1 ]] || die "Usage: $0 <backup-file.sql.gpg>   (see header for env vars)"
BACKUP_FILE="$1"
[[ -f "$BACKUP_FILE" ]] || die "Backup file not found: $BACKUP_FILE"

# Resolve passphrase
if [[ -n "${GPG_PASSPHRASE:-}" ]]; then
  PASS="$GPG_PASSPHRASE"
elif [[ -r "$GPG_PASSPHRASE_FILE" ]]; then
  PASS="$(cat "$GPG_PASSPHRASE_FILE")"
else
  die "No passphrase: set GPG_PASSPHRASE or a readable GPG_PASSPHRASE_FILE ($GPG_PASSPHRASE_FILE)"
fi

SIZE=$(stat -f%z "$BACKUP_FILE" 2>/dev/null || stat -c%s "$BACKUP_FILE" 2>/dev/null || echo 0)
[[ "$SIZE" -gt 0 ]] || die "Backup file is empty"
log "Backup: $BACKUP_FILE (${SIZE} bytes)"

# Run pg_restore, forwarding PGPASSWORD when set so the connection can satisfy a
# scram/md5 pg_hba rule. Three back-ends:
#   DOCKER_NETWORK  -> sidecar client container on the DB's network (real restore
#                      against mm-postgres; reach it via PG_HOST=<container name>)
#   DOCKER_CONTAINER-> docker exec into the DB container (fine for LIST_ONLY, and
#                      for restores where the container accepts local connections)
#   neither         -> local pg_restore binary
run_pg_restore() {
  if [[ -n "$DOCKER_NETWORK" ]]; then
    if [[ -n "${PGPASSWORD:-}" ]]; then
      docker run --rm -i --network "$DOCKER_NETWORK" -e "PGPASSWORD=$PGPASSWORD" "$PG_IMAGE" pg_restore "$@"
    else
      docker run --rm -i --network "$DOCKER_NETWORK" "$PG_IMAGE" pg_restore "$@"
    fi
  elif [[ -n "$DOCKER_CONTAINER" ]]; then
    if [[ -n "${PGPASSWORD:-}" ]]; then
      docker exec -i -e "PGPASSWORD=$PGPASSWORD" "$DOCKER_CONTAINER" pg_restore "$@"
    else
      docker exec -i "$DOCKER_CONTAINER" pg_restore "$@"
    fi
  else
    pg_restore "$@"
  fi
}

decrypt() {
  gpg --batch --quiet --passphrase "$PASS" --decrypt "$BACKUP_FILE"
}

# --- LIST_ONLY: integrity / drill, no writes -------------------------------
# `pg_restore --list` reads the archive from stdin and does NOT open a DB
# connection, so no host/user/password is needed here.
if [[ -n "$LIST_ONLY" ]]; then
  log "LIST_ONLY: verifying archive is intact + restorable (no data written)"
  COUNT=$(decrypt | run_pg_restore --list | grep -cE ' TABLE | INDEX | SEQUENCE | VIEW ' || true)
  [[ "$COUNT" -gt 0 ]] || die "pg_restore --list found 0 restorable objects (archive unreadable?)"
  log "OK — archive lists ${COUNT} restorable objects."
  exit 0
fi

# --- Real restore ----------------------------------------------------------
log "WARNING: restoring into ${PG_DB} as ${PG_USER}${DOCKER_CONTAINER:+ (container ${DOCKER_CONTAINER})}"
log "WARNING: existing data in ${PG_DB} may be overwritten. Stop the consumer first."
if [[ -t 0 ]]; then
  read -rp "Continue? [y/N] " CONFIRM
  [[ "$CONFIRM" =~ ^[Yy]$ ]] || { log "Aborted."; exit 0; }
fi

RESTORE_ARGS=(-h "$PG_HOST" -p "$PG_PORT" --no-owner --no-acl -U "$PG_USER" -d "$PG_DB")
[[ -n "$CLEAN" ]] && RESTORE_ARGS+=(--clean --if-exists)

log "Restoring..."
decrypt | run_pg_restore "${RESTORE_ARGS[@]}" || die "Restore failed"
log "Restore complete."
exit 0
