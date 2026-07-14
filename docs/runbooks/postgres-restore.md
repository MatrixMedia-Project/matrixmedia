# Postgres Backup + Restore Runbook

**Audience:** on-call operator restoring `mm-postgres` from a nightly backup.
**Target:** `${MM_DOMAIN}` (single-host docker compose stack at `/opt/MatrixMedia/`).

> **Set these first** — every command below uses them:
> ```bash
> export MM_SSH=operator@your-server.example.com   # SSH target with sudo
> export MM_DOMAIN=matrix.example.com              # your MM homeserver domain
> ```
**Last drill:** 2026-04-30 — survey complete, live restore drill BLOCKED on credential-handling policy (see [Open questions](#open-questions)).

---

## Backup mechanism (current state)

A systemd timer runs nightly at 03:30 UTC and writes a GPG-symmetric AES256 encrypted custom-format `pg_dump` to `/opt/MatrixMedia/backups/mm-postgres/`. Retention is 30 days; older dumps are unlinked by `find -mtime +30`.

| Artifact | Path |
|---|---|
| Timer | `/etc/systemd/system/mm-postgres-backup.timer` |
| Service | `/etc/systemd/system/mm-postgres-backup.service` |
| Script | `/opt/MatrixMedia/scripts/mm-postgres-backup.sh` |
| Log | `/opt/MatrixMedia/logs/mm-postgres-backup.log` |
| Backups | `/opt/MatrixMedia/backups/mm-postgres/matrixmedia_YYYYMMDD_HHMMSS.sql.gpg` |
| GPG passphrase | `/opt/MatrixMedia/secrets/mm_db_backup_passphrase` (root-readable, 0600) |
| DB admin password | `/opt/MatrixMedia/secrets/mm_db_admin_password` (root-readable, 0600) |
| DB container | `matrixmedia-mm-postgres-1` on docker network `matrixmedia_mm-db-net` |
| Database | `matrixmedia` (admin user `mm_admin`) |

### Verify backups are healthy

```bash
# Most recent dump should be < 36h old and > 100KB.
ssh ${MM_SSH} 'sudo ls -lah /opt/MatrixMedia/backups/mm-postgres/ | tail -3'

# Tail of the backup log shows nightly "backup OK" lines.
ssh ${MM_SSH} 'sudo tail -5 /opt/MatrixMedia/logs/mm-postgres-backup.log'

# Timer is active.
ssh ${MM_SSH} 'sudo systemctl status mm-postgres-backup.timer'
```

---

## Restore — drill (non-destructive, runs alongside production)

Used to verify a backup is restorable without touching production data. Spins up a SECOND postgres container on a non-default port and restores the latest dump into it.

> **WARNING:** The GPG passphrase must NOT leave the production host. The drill below is designed to keep the passphrase in-process on the prod host — the decrypted dump never touches disk on either side. If your security policy forbids running ad-hoc containers on the prod host, see [Drill on a separate host](#drill-on-a-separate-host).

### Prerequisites
- SSH access to `${MM_SSH}` with `sudo`.
- `docker` available on prod host (already installed).
- Free TCP port `127.0.0.1:55432` (loopback only — drill DB never gets external traffic).

### Steps

```bash
# Run end-to-end on the prod host. The passphrase is captured into a shell
# variable inside the sudo bash heredoc, used immediately in a pipeline,
# and never written to disk.
ssh ${MM_SSH} 'sudo bash -se' <<'REMOTE'
set -euo pipefail

LATEST=$(ls -t /opt/MatrixMedia/backups/mm-postgres/matrixmedia_*.sql.gpg | head -1)
echo "Drill source: $LATEST"

GPG_PASS=$(cat /opt/MatrixMedia/secrets/mm_db_backup_passphrase)

# 1. Spin up an ephemeral postgres bound to localhost only.
docker rm -f mm-restore-drill 2>/dev/null || true
docker run -d --name mm-restore-drill \
  -e POSTGRES_PASSWORD=drill \
  -p 127.0.0.1:55432:5432 \
  postgres:16-alpine

# 2. Wait for it to accept connections.
for i in $(seq 1 30); do
  if docker exec mm-restore-drill pg_isready -U postgres >/dev/null 2>&1; then break; fi
  sleep 1
done

# 3. Create the target database.
docker exec mm-restore-drill psql -U postgres -c 'CREATE DATABASE matrixmedia;'

# 4. Decrypt + restore in a single pipeline. Custom-format dump → pg_restore.
gpg --batch --yes --passphrase "$GPG_PASS" --decrypt "$LATEST" \
  | docker exec -i mm-restore-drill pg_restore -U postgres -d matrixmedia --no-owner --no-privileges

# 5. Verification queries — key tables for the pilot.
echo "--- table row counts ---"
docker exec mm-restore-drill psql -U postgres -d matrixmedia -c "
  SELECT 'mm_creator_profiles', count(*) FROM mm_creator_profiles
  UNION ALL SELECT 'mm_donations',        count(*) FROM mm_donations
  UNION ALL SELECT 'mm_streams',          count(*) FROM mm_streams
  UNION ALL SELECT 'mm_recordings',       count(*) FROM mm_recordings
  UNION ALL SELECT 'mm_room_config',      count(*) FROM mm_room_config;
"

echo "--- latest donation BOLT11 (LN proof check) ---"
docker exec mm-restore-drill psql -U postgres -d matrixmedia -c "
  SELECT id, amount_cents, payment_hash IS NOT NULL AS has_proof, created_at
  FROM mm_donations
  WHERE bolt11 IS NOT NULL
  ORDER BY created_at DESC LIMIT 5;
"

# 6. Tear down.
docker rm -f mm-restore-drill
echo "drill OK"
REMOTE
```

### Pass criteria

- All 5 key tables present and row counts > 0 (or match operator expectation for current pilot scale).
- At least one row in `mm_donations` shows `has_proof=t` (confirms LN proof column survived restore).
- Pipeline exits with `drill OK` and no `pg_restore` error or warning to stderr.

### What you're verifying
- Backup file is intact (gpg decrypts cleanly).
- Custom-format dump is well-formed (`pg_restore` finishes without errors).
- Schema migrations through V019 are captured.
- Real row data — not just table definitions.

---

## Restore — production recovery (DESTRUCTIVE — replaces live DB)

> **STOP.** Run this only after confirming the live database is unrecoverable. Coordinate with the project owner before proceeding. Streams in flight will drop; users with open recording sessions will lose any unfinished writes since the last backup (RPO = up to 24 hours).

### Prerequisites
- All [drill](#restore--drill-non-destructive-runs-alongside-production) verifications passed against the same dump file.
- Project owner has approved data loss window.
- Maintenance announcement posted to homeserver welcome room (~5 min head time).

### Steps

```bash
ssh ${MM_SSH} 'sudo bash -se' <<'REMOTE'
set -euo pipefail

LATEST=$(ls -t /opt/MatrixMedia/backups/mm-postgres/matrixmedia_*.sql.gpg | head -1)
echo "Restoring from: $LATEST"

GPG_PASS=$(cat /opt/MatrixMedia/secrets/mm_db_backup_passphrase)
ADMIN_PW=$(cat /opt/MatrixMedia/secrets/mm_db_admin_password)

# 1. Stop services that write to the DB. mm-postgres itself stays up.
cd /opt/MatrixMedia
docker compose stop mm-core mm-server mm-payment 2>/dev/null || \
  docker compose stop mm-core   # adjust to actual service names if list shifts

# 2. Drop and recreate the database (DESTRUCTIVE).
docker exec matrixmedia-mm-postgres-1 \
  psql -U mm_admin -d postgres -c \
  "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
   WHERE datname='matrixmedia' AND pid <> pg_backend_pid();"
docker exec matrixmedia-mm-postgres-1 \
  psql -U mm_admin -d postgres -c 'DROP DATABASE IF EXISTS matrixmedia;'
docker exec matrixmedia-mm-postgres-1 \
  psql -U mm_admin -d postgres -c 'CREATE DATABASE matrixmedia OWNER mm_admin;'

# 3. Restore.
gpg --batch --yes --passphrase "$GPG_PASS" --decrypt "$LATEST" \
  | docker run --rm -i --network matrixmedia_mm-db-net \
      -e PGPASSWORD="$ADMIN_PW" postgres:16-alpine \
      pg_restore -h matrixmedia-mm-postgres-1 -U mm_admin -d matrixmedia \
        --no-owner --no-privileges --single-transaction

# 4. Restart services.
docker compose start mm-core mm-server mm-payment 2>/dev/null || \
  docker compose start mm-core
echo "restore complete; verify health endpoints next"
REMOTE
```

### Post-restore verification

```bash
# All neighbours healthy.
for url in \
  https://${MM_DOMAIN}/_matrix/client/versions \
  https://${MM_DOMAIN}/_mm/switch/health \
  https://${MM_NEIGHBOR_DOMAIN} \
  https://${MM_ROOT_DOMAIN}; do
  printf "%s  %s\n" "$(curl -sk -o /dev/null -w '%{http_code}' --max-time 6 "$url")" "$url"
done

# mm-core can read profile + donation.
curl -sk https://${MM_DOMAIN}/_mm/admin/v1/lightning-stats | jq .
```

If any neighbour shows non-2xx that wasn't pre-existing: `docker compose logs mm-core | tail -50` and triage.

---

## Drill on a separate host

If your security policy forbids ad-hoc docker containers on the prod host:

1. `scp` the `.sql.gpg` file to a dev machine (encrypted; safe to transit).
2. Pull the GPG passphrase only when needed, into an env var, never to disk: `GPG_PASS=$(ssh ${MM_SSH} 'sudo cat /opt/MatrixMedia/secrets/mm_db_backup_passphrase')`.
3. Run steps 1-6 of the [drill](#restore--drill-non-destructive-runs-alongside-production) section locally with the same docker pattern.
4. Wipe `~/.bash_history`, `unset GPG_PASS`, and `docker rm -f mm-restore-drill` before logging out of the dev machine.

This path leaks the passphrase into a second host's RAM (briefly) — operator's call vs the in-prod ephemeral container approach.

---

## Open questions

- **2026-04-30:** Live drill not yet executed end-to-end. Backup mechanism is verified (script reviewed, daily files growing 110→129KB, 30-day window populated). The pass criteria above need to be exercised at least once before the pilot opens. Two paths blocked by policy: (a) extracting the GPG passphrase to a laptop (denied as a Production Read of credentials), (b) running the in-prod ephemeral container (production state change). Owner needs to pick a path and grant scope.

## Related

- `/opt/MatrixMedia/scripts/mm-postgres-backup.sh` — the script the timer runs.
- `docs/disaster-recovery.md` — broader DR topology (S3 media, Matrix state recovery).
- `docs/operations-runbook.md` — day-2 ops, triage, neighbour validation pattern.
