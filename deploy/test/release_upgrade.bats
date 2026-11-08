load helper

# Moving an install to a new release. `mmctl upgrade` only re-pulled the pins already in
# /opt/mm/versions.env; only install.sh copies in a new set. So the one way to a new release
# was re-running the one-liner, which also migrated the database without any backup.
#
# `[[ ]] || false` throughout: on bash < 4.1 (macOS /bin/bash) a failing [[ ]] that is not
# a test's last command does not fail the test.

setup() {
  setup_tmp
  source "$DEPLOY_ROOT/lib/common.sh"
  source "$DEPLOY_ROOT/lib/secrets.sh"
  source "$DEPLOY_ROOT/lib/backup.sh"
  source "$DEPLOY_ROOT/lib/lifecycle.sh"
  DC=(true)   # stub: no docker in unit tests
  T="$(mktemp -d)"
  mkdir -p "$T/bin" "$MM_ROOT/backups" "$MM_ROOT/src"
  : > "$MM_ROOT/src/OLD"       # the tree the installed release was run from
  cat > "$MM_ROOT/.env" <<EOF
MM_DOMAIN=example.com
ACME_EMAIL=a@b.co
MM_DEMO_MODE=false
MM_TEMP_MODE=false
EOF
  echo 'MM_VERSION=0.10.0' > "$MM_ROOT/versions.env"
  _release 0.11.0
  # Fake curl: logs the URL, writes the release tarball to -o, or fails when told to.
  cat > "$T/bin/curl" <<FAKE
#!/usr/bin/env bash
echo "\$*" >> "$T/curl.log"
[ -z "\${FAKE_CURL_FAIL:-}" ] || exit 22
out=""
while [ \$# -gt 0 ]; do [ "\$1" = "-o" ] && { out="\$2"; shift; }; shift; done
[ -z "\$out" ] || cp "$T/release.tar.gz" "\$out"
FAKE
  chmod +x "$T/bin/curl"; : > "$T/curl.log"
  PATH="$T/bin:$PATH"; export PATH
  backup() { _mk_backup 20260714-120000; }
}
teardown() { teardown_tmp; rm -rf "$T"; }

# _release VERSION -- the tarball GitHub serves for a tag: one top-level directory, whose
# deploy/install.sh here only records how it was run.
_release() {
  rm -rf "$T/pkg"; mkdir -p "$T/pkg/matrixmedia-x/deploy"
  cat > "$T/pkg/matrixmedia-x/deploy/install.sh" <<'INSTALLER'
#!/usr/bin/env bash
echo "ARGS $*" >> "$MM_ROOT/installer.log"
echo "BACKUP_TAKEN ${MM_BACKUP_TAKEN:-}" >> "$MM_ROOT/installer.log"
[ -z "${FAKE_INSTALL_FAIL:-}" ] || exit 1
INSTALLER
  echo "MM_VERSION=$1" > "$T/pkg/matrixmedia-x/deploy/versions.env"
  tar -czf "$T/release.tar.gz" -C "$T/pkg" matrixmedia-x
}
_mk_backup() {   # _mk_backup TS -- a complete, fresh backup set
  : > "$MM_ROOT/backups/config-$1.tar.gz"
  : > "$MM_ROOT/backups/pg-app-$1.sql.gz"
  : > "$MM_ROOT/backups/pg-synapse-$1.sql.gz"
}

# ── mmctl upgrade --to <tag> ─────────────────────────────────────────────────

@test "upgrade --to downloads the tag, backs up, swaps the tree and runs that release's installer" {
  run mm_upgrade_to v0.11.0
  [ "$status" -eq 0 ]
  grep -qF 'https://github.com/MatrixMedia-Project/matrixmedia/archive/v0.11.0.tar.gz' "$T/curl.log"
  [ ! -e "$MM_ROOT/src/OLD" ]
  [ "$(cat "$MM_ROOT/src/.deploy-ref")" = v0.11.0 ]
  [ ! -e "$MM_ROOT/src.new" ]
  grep -q '^ARGS --domain example.com --email a@b.co --non-interactive$' "$MM_ROOT/installer.log"
  # mmctl already backed up: the installer is told so instead of taking a second one
  grep -q "^BACKUP_TAKEN $MM_ROOT/backups/config-20260714-120000.tar.gz$" "$MM_ROOT/installer.log"
  [[ "$output" == *"rollback point"*"config-20260714-120000.tar.gz"* ]] || false
}

@test "upgrade --to re-runs a --demo or --no-domain install in the same mode" {
  printf 'MM_DOMAIN=203-0-113-5.sslip.io\nACME_EMAIL=temp@203-0-113-5.sslip.io\nMM_DEMO_MODE=true\nMM_TEMP_MODE=true\n' > "$MM_ROOT/.env"
  run mm_upgrade_to v0.11.0
  [ "$status" -eq 0 ]
  args="$(grep '^ARGS' "$MM_ROOT/installer.log")"
  [[ "$args" == *"--no-domain"* ]] || false
  [[ "$args" == *"--demo"* ]] || false
  [[ "$args" != *"--domain "* ]] || false     # --no-domain conflicts with --domain
}

@test "a tag that cannot be downloaded changes nothing and takes no backup" {
  export FAKE_CURL_FAIL=1
  backup() { echo BACKUP-RAN; _mk_backup 20260714-120000; }
  run mm_upgrade_to v9.9.9
  [ "$status" -ne 0 ]
  [[ "$output" == *"could not download"* ]] || false
  [[ "$output" == *"nothing was changed"* ]] || false
  [[ "$output" != *"BACKUP-RAN"* ]] || false
  [ -e "$MM_ROOT/src/OLD" ]
  [ ! -e "$MM_ROOT/src.new" ]
}

@test "a failed backup stops the upgrade before the new tree replaces the old one" {
  backup() { return 1; }
  run mm_upgrade_to v0.11.0
  [ "$status" -ne 0 ]
  [[ "$output" == *"BACKUP FAILED"* ]] || false
  [ -e "$MM_ROOT/src/OLD" ]
  [ ! -e "$MM_ROOT/installer.log" ]
  [ ! -e "$MM_ROOT/src.new" ]
}

# The database migrates forward only: an older release's mm-core on a newer schema is
# undefined. The way back is the backup.
@test "upgrade --to refuses an older release, before backing up, and points at restore" {
  echo 'MM_VERSION=0.12.0' > "$MM_ROOT/versions.env"
  backup() { echo BACKUP-RAN; }
  run mm_upgrade_to v0.11.0
  [ "$status" -ne 0 ]
  [[ "$output" == *"older"* ]] || false
  [[ "$output" == *"mmctl restore"* ]] || false
  [[ "$output" != *"BACKUP-RAN"* ]] || false
  [ -e "$MM_ROOT/src/OLD" ]
  [ ! -e "$MM_ROOT/src.new" ]
}

@test "upgrade --to the installed release is allowed (it repairs)" {
  echo 'MM_VERSION=0.11.0' > "$MM_ROOT/versions.env"
  run mm_upgrade_to v0.11.0
  [ "$status" -eq 0 ]
}

@test "a failed installer says the database cannot roll back and names the restore command" {
  export FAKE_INSTALL_FAIL=1
  run mm_upgrade_to v0.11.0
  [ "$status" -ne 0 ]
  [[ "$output" == *"mmctl restore $MM_ROOT/backups/config-20260714-120000.tar.gz"* ]] || false
}

@test "upgrade --to refuses while rented servers exist, before it downloads anything" {
  fleet_runner_guard() { die "2 rented server(s) are running"; }
  run mm_upgrade_to v0.11.0
  [ "$status" -ne 0 ]
  [ ! -s "$T/curl.log" ]
}

@test "upgrade --to with no tag says how to use it" {
  run mm_upgrade_to ""
  [ "$status" -ne 0 ]
  [[ "$output" == *"mmctl upgrade --to <tag>"* ]] || false
  [ ! -s "$T/curl.log" ]
}

@test "version_lt orders release versions numerically, with or without a leading v" {
  version_lt 0.10.0 0.11.0
  version_lt v0.9.9 0.10.0
  version_lt 0.10.0 0.10.1
  ! version_lt 0.11.0 0.10.0
  ! version_lt 0.10.0 0.10.0
  ! version_lt v1.0.0 0.99.99
}

@test "mmctl upgrade takes --to <tag> or --to=<tag>, and rejects anything else" {
  printf '#!/bin/sh\necho "COMPOSE-RAN $*"\n' > "$T/bin/docker"; chmod +x "$T/bin/docker"
  : > "$MM_ROOT/.env.secrets"
  run bash "$DEPLOY_ROOT/mmctl" upgrade --to
  [ "$status" -ne 0 ]
  [[ "$output" == *"mmctl upgrade --to <tag>"* ]] || false
  run bash "$DEPLOY_ROOT/mmctl" upgrade --bogus
  [ "$status" -ne 0 ]
  [[ "$output" == *"--to <tag>"* ]] || false
  [[ "$output" != *"COMPOSE-RAN"* ]] || false
  grep -q -- '--to=\*) *mm_upgrade_to "\${1#--to=}"' "$DEPLOY_ROOT/mmctl"
}

@test "mmctl usage advertises upgrade --to" {
  run bash "$DEPLOY_ROOT/mmctl"
  [[ "$output" == *"upgrade [--to <tag>]"* ]] || false
}

# ── install.sh over an existing install ──────────────────────────────────────
# A plain re-run of the one-liner fetches main, i.e. the newest release, and migrated the
# database with no backup. preinstall_backup runs before install.sh changes anything.

@test "a first install takes no backup (there is nothing to protect yet)" {
  backup() { echo BACKUP-RAN; }
  run preinstall_backup
  [ "$status" -eq 0 ]
  [[ "$output" != *"BACKUP-RAN"* ]] || false
}

@test "a re-run over a completed install backs up first" {
  : > "$MM_ROOT/admin.credentials"      # written only at the end of a completed install
  backup() { echo BACKUP-RAN; }
  run preinstall_backup
  [ "$status" -eq 0 ]
  [[ "$output" == *"BACKUP-RAN"* ]] || false
}

@test "a re-run over a completed install stops when the backup fails, and says how to proceed" {
  : > "$MM_ROOT/admin.credentials"
  backup() { return 1; }
  run preinstall_backup
  [ "$status" -ne 0 ]
  [[ "$output" == *"BACKUP FAILED"* ]] || false
  [[ "$output" == *"mmctl start"* ]] || false
  [[ "$output" == *"MM_SKIP_BACKUP=1"* ]] || false
}

@test "MM_SKIP_BACKUP=1 re-runs without a backup, and warns" {
  : > "$MM_ROOT/admin.credentials"
  backup() { echo BACKUP-RAN; }
  MM_SKIP_BACKUP=1 run preinstall_backup
  [ "$status" -eq 0 ]
  [[ "$output" != *"BACKUP-RAN"* ]] || false
  [[ "$output" == *"WITHOUT a backup"* ]] || false
}

@test "a backup mmctl upgrade --to already took is not taken twice" {
  : > "$MM_ROOT/admin.credentials"
  _mk_backup 20260714-120000
  backup() { echo BACKUP-RAN; }
  MM_BACKUP_TAKEN="$MM_ROOT/backups/config-20260714-120000.tar.gz" run preinstall_backup
  [ "$status" -eq 0 ]
  [[ "$output" != *"BACKUP-RAN"* ]] || false
  [[ "$output" == *"config-20260714-120000.tar.gz"* ]] || false
  # a path that is not there is no backup
  MM_BACKUP_TAKEN="$MM_ROOT/backups/config-nope.tar.gz" run preinstall_backup
  [[ "$output" == *"BACKUP-RAN"* ]] || false
}

@test "install.sh backs up after preflight and before it copies in the new release" {
  pf="$(grep -n '^preflight "\$DOMAIN"$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  bk="$(grep -n '^preinstall_backup$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  cp="$(grep -n '^cp "\$HERE/versions.env" "\$MM_ROOT/"' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  env="$(grep -n '^cat > "\$MM_ROOT/.env" <<EOF$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  [ -n "$pf" ] && [ -n "$bk" ] && [ -n "$cp" ] && [ -n "$env" ]
  [ "$pf" -lt "$bk" ] && [ "$bk" -lt "$cp" ] && [ "$bk" -lt "$env" ]
  ! grep -q '^cp -r "\$HERE/templates"' <(sed -n "1,${bk}p" "$DEPLOY_ROOT/install.sh")
}
