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

MM_SYNAPSE_CONTAINER="matrixmedia-synapse-1"
# The volumes `mmctl backup --media` archives (compose project "matrixmedia").
MM_MEDIA_VOLUMES="synapse-data recordings-data"

# The file names backup() writes. Retention deletes only these; anything else an operator
# keeps in backups/ is left alone.
MM_BACKUP_PATTERNS="config-*.tar.gz pg-app-*.sql.gz pg-synapse-*.sql.gz media-*.tar"

# _backup_dir -- create backups/ as 0700, and tighten backups an older release wrote 0644.
#
# Every backup holds .env.secrets (and the config tarball the signing key): it is as
# sensitive as the secrets themselves. $MM_ROOT is 0700 too; this does not lean on that.
_backup_dir() {
  local d="$MM_ROOT/backups" p
  if ! { mkdir -p "$d" && chmod 700 "$d"; }; then warn "cannot create $d"; return 1; fi
  for p in $MM_BACKUP_PATTERNS; do
    find "$d" -maxdepth 1 -type f -name "$p" ! -perm 600 -exec chmod 600 {} + 2>/dev/null || true
  done
}

# _env_value KEY -- KEY's value in $MM_ROOT/.env, or empty.
_env_value() { sed -n "s/^$1=//p" "$MM_ROOT/.env" 2>/dev/null | head -1; }

# _signing_key_to OUT -- copy Synapse's signing key out of its volume into OUT (0600).
#
# The key is the server's federation identity and lives in the synapse-data volume, which
# the config + database backup did not cover: a server rebuilt from its backups came back
# under a new key. It is read through the running container, not the volume's host path,
# which depends on Docker's data-root. Unreadable (Synapse down) warns and returns 1: the
# key never changes in an upgrade, so the backup is still a rollback, just not a complete
# disaster-recovery copy.
_signing_key_to() {
  local out="$1" domain
  domain="$(_env_value MM_DOMAIN)"
  if [ -n "$domain" ] \
     && ( umask 077; docker exec "$MM_SYNAPSE_CONTAINER" cat "/data/$domain.signing.key" > "$out" ) 2>/dev/null \
     && [ -s "$out" ]; then
    return 0
  fi
  warn "could not read Synapse's signing key (/data/${domain:-<domain>}.signing.key in $MM_SYNAPSE_CONTAINER): this backup does not hold the server's federation identity. Is synapse running? (mmctl status)"
  return 1
}

# backup_configs TS -- tar the config dir, env files, image pins and compose file (those
# that exist), plus Synapse's signing key, into one 0600 tarball.
#
# versions.env and docker-compose.yml are in it because a release upgrade replaces them:
# restoring the old config and databases under the NEW pins would start the new images on
# the old database, and they would migrate it forward again.
backup_configs() {
  local ts="$1"
  _backup_dir || return 1
  local items=() key=() f out="$MM_ROOT/backups/config-$ts.tar.gz" stage
  for f in config .env .env.secrets versions.env docker-compose.yml; do [ -e "$MM_ROOT/$f" ] && items+=("$f"); done
  stage="$(mktemp -d "$MM_ROOT/backups/.stage.XXXXXX")" || { warn "config backup FAILED (no temp dir)"; return 1; }
  if _signing_key_to "$stage/synapse.signing.key"; then key=(-C "$stage" synapse.signing.key); fi

  # Check tar. This used to be a bare `tar czf ...` followed by `log ...`, and because the
  # function is invoked as `backup_configs || rc=1` (which disables errexit inside it), a
  # failing tar fell through to `log`, whose exit 0 became the return value. On a full disk
  # that leaves a TRUNCATED tarball with a current mtime — which every freshness check then
  # trusts as a rollback point.
  # ${key[@]+...}: an empty array is "unbound" under set -u on bash < 4.4 (macOS bash 3.2).
  if ! ( umask 077; tar czf "$out" -C "$MM_ROOT" "${items[@]}" ${key[@]+"${key[@]}"} ); then
    rm -rf "$stage"
    rm -f "$out"        # leave no partial file for a freshness check to believe in
    warn "config backup FAILED (tar)"
    return 1
  fi
  rm -rf "$stage"
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
  if ! ( set -o pipefail; umask 077
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
  _backup_dir || return 1
  dump_one "$MM_PG_SYNAPSE_CONTAINER" "$MM_PG_SYNAPSE_SUPERUSER" "$MM_PG_SYNAPSE_DB" \
           "$MM_ROOT/backups/pg-synapse-$ts.sql.gz" || rc=1
  dump_one "$MM_PG_APP_CONTAINER" "$MM_PG_APP_SUPERUSER" "$MM_PG_APP_DB" \
           "$MM_ROOT/backups/pg-app-$ts.sql.gz" || rc=1
  return "$rc"
}

# backup_media TS -- archive the synapse-data and recordings-data volumes (opt-in: --media).
#
# Uploaded media and recordings can be far larger than everything else together, so a plain
# backup leaves them out. A throwaway container mounts both volumes read-only and streams a
# tar out; the image is the pinned Postgres one, which every install has already pulled.
# Plain tar, not gzip: the files are already compressed video and images.
backup_media() {
  local ts="$1" out img v mounts=() names=()
  _backup_dir || return 1
  out="$MM_ROOT/backups/media-$ts.tar"
  img="$(sed -n 's/^POSTGRES_IMAGE=//p' "$MM_ROOT/versions.env" 2>/dev/null | head -1)"
  for v in $MM_MEDIA_VOLUMES; do
    mounts+=(-v "matrixmedia_$v:/backup/$v:ro"); names+=("$v")
  done
  if ! ( set -o pipefail; umask 077
         docker run --rm --entrypoint tar "${mounts[@]}" "${img:-postgres:18-alpine}" \
           cf - -C /backup "${names[@]}" > "$out" ); then
    rm -f "$out"
    warn "media backup FAILED (volumes: $MM_MEDIA_VOLUMES)"
    return 1
  fi
  log "media backup: $out ($(du -h "$out" | cut -f1))"
}

# backup_keep_days -- how many days of backups to keep: MM_BACKUP_KEEP_DAYS from the
# environment, else from $MM_ROOT/.env, else 14. 0 keeps every backup.
backup_keep_days() {
  local v="${MM_BACKUP_KEEP_DAYS:-}"
  [ -n "$v" ] || v="$(_env_value MM_BACKUP_KEEP_DAYS)"
  echo "${v:-14}"
}

# prune_backups DAYS [KEEP_TS] -- delete backup files older than DAYS days (by mtime).
# Only names backup() writes are considered, and never the files of KEEP_TS.
# Anything but a whole number deletes nothing: retention must never guess.
prune_backups() {
  local days="$1" keep="${2:-}" f p n=0 named=()
  case "$days" in ''|*[!0-9]*)
    warn "MM_BACKUP_KEEP_DAYS=$days is not a whole number of days; no backup was deleted"
    return 0 ;;
  esac
  [ "$days" -gt 0 ] || return 0
  for p in $MM_BACKUP_PATTERNS; do named+=(-o -name "$p"); done
  while IFS= read -r f; do
    if [ -n "$keep" ]; then case "$f" in *"-$keep."*) continue ;; esac; fi
    rm -f "$f" && n=$(( n + 1 ))
  done < <(find "$MM_ROOT/backups" -maxdepth 1 -type f \( "${named[@]:1}" \) -mmin +$(( days * 1440 )) 2>/dev/null)
  if [ "$n" -gt 0 ]; then log "deleted $n backup file(s) older than $days days (MM_BACKUP_KEEP_DAYS)"; fi
}

# backup [--media] -- config + both databases (+ the media volumes with --media), under ONE
# timestamp. Non-zero if ANY part failed. Only a fully successful backup prunes old ones:
# a failing backup must not also delete the last good ones.
#
# The timestamp is taken once and passed down. It used to be generated independently inside
# backup_configs and backup_db, so a tar that happened to cross a second boundary — routine
# — left the DB dumps under a different suffix than the config tarball. Everything
# downstream (upgrade's rollback check, restore's pairing) finds the dumps BY the config
# tarball's timestamp, and would simply not find them.
#
# The return value is load-bearing: `mmctl upgrade` refuses to proceed when it is non-zero,
# because the backup IS the rollback (the DB rolls forward only).
# shellcheck disable=SC2120  # mmctl passes --media; shellcheck cannot see through its `source`
backup() {
  local rc=0 ts media=0 a
  for a in "$@"; do
    case "$a" in
      --media) media=1 ;;
      *) die "unknown option $a (usage: mmctl backup [--media])" ;;
    esac
  done
  ts="$(date -u +%Y%m%d-%H%M%S)"
  backup_configs "$ts" || rc=1
  backup_db "$ts"      || rc=1
  if [ "$media" -eq 1 ]; then backup_media "$ts" || rc=1; fi
  if [ "$rc" -eq 0 ]; then prune_backups "$(backup_keep_days)" "$ts"; fi
  return "$rc"
}

# install_completed -- 0 when an earlier install.sh run on this host reached its end
# (admin.credentials is its last write). A half-finished first install has no data to lose.
install_completed() { [ -f "$MM_ROOT/admin.credentials" ]; }

# preinstall_backup -- back up a completed install before install.sh changes it.
#
# A re-run of the one-liner fetches main, the newest release, and its mm-core migrates the
# database forward on start: that is an upgrade, and the backup is its only way back (the
# rollback contract in lifecycle.sh). So it refuses to go on without one, unless
# MM_SKIP_BACKUP=1. MM_BACKUP_TAKEN names a backup `mmctl upgrade --to` took just before
# running this installer, so it is not taken twice.
preinstall_backup() {
  install_completed || return 0
  if [ -n "${MM_BACKUP_TAKEN:-}" ] && [ -f "$MM_BACKUP_TAKEN" ]; then
    log "re-run: using the backup mmctl upgrade just took: $MM_BACKUP_TAKEN"
    return 0
  fi
  if [ "${MM_SKIP_BACKUP:-0}" = 1 ]; then
    warn "MM_SKIP_BACKUP=1: re-running over an existing install WITHOUT a backup. If this moves to a newer release, the database migrates forward and there is no way back."
    return 0
  fi
  log "re-run over an existing install: backing up first. A newer release migrates the database forward only, so this backup is the way back (mmctl restore)."
  backup || die "BACKUP FAILED, so nothing was changed. The databases must be running to back up: start the stack (mmctl start) and re-run, or set MM_SKIP_BACKUP=1 to re-run without a backup."
}
