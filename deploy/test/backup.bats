load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/backup.sh"
  mkdir -p "$MM_ROOT/config"; echo x > "$MM_ROOT/config/a.yaml"; echo y > "$MM_ROOT/.env"
}
teardown() { teardown_tmp; }

@test "backup_configs writes a tar.gz containing config + .env" {
  run backup_configs 20260714-000000; [ "$status" -eq 0 ]
  local f; f="$(ls "$MM_ROOT"/backups/*.tar.gz | head -1)"
  tar tzf "$f" | grep -q 'config/a.yaml'
}
@test "backup_configs tolerates a missing .env.secrets" {
  rm -f "$MM_ROOT/.env.secrets"
  run backup_configs 20260714-000000; [ "$status" -eq 0 ]
}

# `[[ ]] || false` throughout: on bash < 4.1 (macOS /bin/bash) a failing [[ ]] that is
# not a test's last command does not fail the test.

# docker stand-in: Synapse's container answers the signing-key read, any other exec is a
# pg_dump (a few hundred bytes, enough to pass dump_one's size floor), `run` is the media
# archive. Every call is logged.
_stub_docker() {
  docker() {
    echo "$*" >> "$MM_ROOT/docker.log"
    case "$1 $2" in
      "exec matrixmedia-synapse-1") echo "ed25519 a_TEST signingkeymaterial" ;;
      "exec "*) head -c 600 /dev/urandom ;;
      "run "*)  printf 'media-archive' ;;
    esac
  }
}
_age() {   # _age SECONDS FILE... -- set the files' mtime SECONDS in the past
  # Local time, not -u: `touch -t` reads its argument as local time.
  local when; when=$(( $(date +%s) - $1 )); shift
  touch -t "$(date -r "$when" +%Y%m%d%H%M.%S 2>/dev/null || date -d "@$when" +%Y%m%d%H%M.%S)" "$@"
}
_mk_set() {   # _mk_set TS AGE_SECS -- a complete backup set (config, both dumps, media)
  local f
  mkdir -p "$MM_ROOT/backups"
  for f in "config-$1.tar.gz" "pg-app-$1.sql.gz" "pg-synapse-$1.sql.gz" "media-$1.tar"; do
    : > "$MM_ROOT/backups/$f"; _age "$2" "$MM_ROOT/backups/$f"
  done
}
DAY=86400

# ── permissions: a backup holds every secret ─────────────────────────────────
@test "the config backup is written 0600 inside a 0700 backups directory" {
  backup_configs 20260714-000000 2>/dev/null
  [ "$(file_mode "$MM_ROOT/backups/config-20260714-000000.tar.gz")" = 600 ]
  [ "$(file_mode "$MM_ROOT/backups")" = 700 ]
}
@test "database dumps are written 0600" {
  _stub_docker
  backup_db 20260714-000000 2>/dev/null
  [ "$(file_mode "$MM_ROOT/backups/pg-app-20260714-000000.sql.gz")" = 600 ]
  [ "$(file_mode "$MM_ROOT/backups/pg-synapse-20260714-000000.sql.gz")" = 600 ]
}
@test "a backup run tightens the 0644 backups an older release left behind" {
  _stub_docker
  _mk_set 20260701-000000 $(( 2 * DAY ))
  chmod 644 "$MM_ROOT"/backups/*
  backup 2>/dev/null
  for f in "$MM_ROOT"/backups/*; do
    [ "$(file_mode "$f")" = 600 ] || { echo "$f is $(file_mode "$f")"; return 1; }
  done
}

# ── what a config backup holds ───────────────────────────────────────────────
# A release upgrade replaces versions.env and docker-compose.yml. Restoring the config
# and databases without the old pins would start the NEW images on the OLD database.
@test "the config backup carries versions.env and docker-compose.yml" {
  echo 'MM_VERSION=0.10.0' > "$MM_ROOT/versions.env"; echo 'services: {}' > "$MM_ROOT/docker-compose.yml"
  backup_configs 20260714-000000 2>/dev/null
  run tar tzf "$MM_ROOT/backups/config-20260714-000000.tar.gz"
  [[ "$output" == *"versions.env"* ]] || false
  [[ "$output" == *"docker-compose.yml"* ]] || false
}
# The signing key lives in the synapse-data volume, which no backup covered: a server
# rebuilt from backups came back with a new federation identity.
@test "the config backup carries Synapse's signing key, and leaves no copy behind" {
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  _stub_docker
  backup_configs 20260714-000000 2>/dev/null
  [ "$(tar xzOf "$MM_ROOT/backups/config-20260714-000000.tar.gz" synapse.signing.key)" = "ed25519 a_TEST signingkeymaterial" ]
  grep -q 'exec matrixmedia-synapse-1 cat /data/example.com.signing.key' "$MM_ROOT/docker.log"
  [ "$(ls -A "$MM_ROOT/backups")" = "config-20260714-000000.tar.gz" ]
}
@test "an unreadable signing key warns, but the config backup still succeeds" {
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  docker() { return 1; }
  run backup_configs 20260714-000000
  [ "$status" -eq 0 ]
  [[ "$output" == *"signing key"* ]] || false
  [ -f "$MM_ROOT/backups/config-20260714-000000.tar.gz" ]
}

# ── retention ────────────────────────────────────────────────────────────────
@test "prune_backups deletes backup files older than the retention and keeps the rest" {
  _mk_set 20260601-000000 $(( 20 * DAY ))
  _mk_set 20260705-000000 $(( 2 * DAY ))
  : > "$MM_ROOT/backups/my-notes.txt"; _age $(( 20 * DAY )) "$MM_ROOT/backups/my-notes.txt"
  prune_backups 14 2>/dev/null
  [ -z "$(ls "$MM_ROOT/backups" | grep 20260601 || true)" ]
  [ "$(ls "$MM_ROOT/backups" | grep -c 20260705)" -eq 4 ]
  [ -f "$MM_ROOT/backups/my-notes.txt" ]      # not ours: never touched
}
@test "prune_backups 0 keeps every backup" {
  _mk_set 20250101-000000 $(( 300 * DAY ))
  prune_backups 0 2>/dev/null
  [ "$(ls "$MM_ROOT/backups" | grep -c 20250101)" -eq 4 ]
}
@test "a retention that is not a whole number of days deletes nothing, and says so" {
  _mk_set 20250101-000000 $(( 300 * DAY ))
  run prune_backups two
  [ "$status" -eq 0 ]
  [[ "$output" == *"MM_BACKUP_KEEP_DAYS"* ]] || false
  [ "$(ls "$MM_ROOT/backups" | grep -c 20250101)" -eq 4 ]
}
@test "the retention comes from the environment, else .env, else 14 days" {
  [ "$(backup_keep_days)" = 14 ]
  printf 'MM_BACKUP_KEEP_DAYS=30\n' >> "$MM_ROOT/.env"
  [ "$(backup_keep_days)" = 30 ]
  [ "$(MM_BACKUP_KEEP_DAYS=7 backup_keep_days)" = 7 ]
}
@test "a successful backup prunes old backups" {
  _stub_docker
  _mk_set 20260601-000000 $(( 20 * DAY ))
  run backup
  [ "$status" -eq 0 ]
  [ -z "$(ls "$MM_ROOT/backups" | grep 20260601 || true)" ]
  [ "$(ls "$MM_ROOT/backups" | grep -c '^config-')" -eq 1 ]   # the one just taken
}
# A failing backup must not also delete the last good ones.
@test "a failed backup deletes nothing" {
  _mk_set 20260601-000000 $(( 20 * DAY ))
  docker() { return 1; }
  run backup
  [ "$status" -ne 0 ]
  [ "$(ls "$MM_ROOT/backups" | grep -c 20260601)" -eq 4 ]
}

# ── media (opt-in) ───────────────────────────────────────────────────────────
@test "backup --media also archives both media volumes, read-only, into a 0600 file" {
  _stub_docker
  run backup --media
  [ "$status" -eq 0 ]
  m="$(ls "$MM_ROOT"/backups/media-*.tar)"
  [ "$(cat "$m")" = media-archive ]
  [ "$(file_mode "$m")" = 600 ]
  run grep '^run ' "$MM_ROOT/docker.log"
  [[ "$output" == *"matrixmedia_synapse-data:/backup/synapse-data:ro"* ]] || false
  [[ "$output" == *"matrixmedia_recordings-data:/backup/recordings-data:ro"* ]] || false
}
@test "a plain backup does not touch the media volumes" {
  _stub_docker
  run backup
  [ "$status" -eq 0 ]
  [ -z "$(ls "$MM_ROOT"/backups/media-* 2>/dev/null || true)" ]
  ! grep -q '^run ' "$MM_ROOT/docker.log"
}
@test "a failed media archive fails the backup and leaves no partial file" {
  docker() {   # as _stub_docker, but the media archive fails part-way
    case "$1 $2" in
      "exec "*) head -c 600 /dev/urandom ;;
      "run "*)  printf 'partial'; return 1 ;;
    esac
  }
  run backup --media
  [ "$status" -ne 0 ]
  [ -z "$(ls "$MM_ROOT"/backups/media-* 2>/dev/null || true)" ]
}
@test "backup rejects an option it does not know" {
  run backup --bogus
  [ "$status" -ne 0 ]
  [[ "$output" == *"--media"* ]] || false
}
@test "mmctl backup hands --media through" {
  mkdir -p "$MM_ROOT/bin"
  cat > "$MM_ROOT/bin/docker" <<'STUB'
#!/bin/sh
echo "$*" >> "$MM_ROOT/docker.log"
case "$1" in exec) head -c 600 /dev/urandom ;; run) printf media ;; esac
STUB
  chmod +x "$MM_ROOT/bin/docker"; : > "$MM_ROOT/.env.secrets"
  run env PATH="$MM_ROOT/bin:$PATH" MM_ROOT="$MM_ROOT" bash "$DEPLOY_ROOT/mmctl" backup --media
  [ "$status" -eq 0 ]
  ls "$MM_ROOT"/backups/media-*.tar
}

# install.sh rewrites .env on every run; a key it does not carry forward is lost.
@test "install.sh keeps MM_BACKUP_KEEP_DAYS when it rewrites .env" {
  carry="$(sed -n '/^if \[ -f "\$MM_ROOT\/.env" \]; then$/,/^fi$/p' "$DEPLOY_ROOT/install.sh")"
  [[ "$carry" == *"MM_BACKUP_KEEP_DAYS"* ]] || false
  persist="$(sed -n '/^for v in .*MM_REGISTRY/,/^done$/p' "$DEPLOY_ROOT/install.sh")"
  [[ "$persist" == *"MM_BACKUP_KEEP_DAYS"* ]] || false
  [[ "$persist" == *'echo "$v=${!v}" >> "$MM_ROOT/.env"'* ]] || false
}

# A backup killed mid-tar left backups/.stage.* (with the signing key in it) behind for good.
@test "a backup clears stage directories an interrupted backup left, but not a running one's" {
  mkdir -p "$MM_ROOT/backups/.stage.OLD" "$MM_ROOT/backups/.stage.NEW"
  : > "$MM_ROOT/backups/.stage.OLD/synapse.signing.key"
  _age 7200 "$MM_ROOT/backups/.stage.OLD"
  backup_configs 20260714-000000 2>/dev/null
  [ ! -e "$MM_ROOT/backups/.stage.OLD" ]
  [ -d "$MM_ROOT/backups/.stage.NEW" ]
}
