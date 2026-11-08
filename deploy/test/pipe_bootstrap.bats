load helper

# `curl …/install.sh | sudo bash` hands bash the script text with no file behind it, so
# install.sh has no deploy/ beside it to source lib/*.sh from. Before this, the README's
# one-liner died on its first line on every host; only clone-then-run worked.

setup() {
  setup_tmp
  T="$(mktemp -d)"
  # Stand-in for GitHub's source tarball: this deploy/ under one top-level directory,
  # the way the archive endpoint lays a repo out.
  mkdir -p "$T/pkg/matrixmedia-ref" "$T/bin" "$T/empty"
  cp -R "$DEPLOY_ROOT" "$T/pkg/matrixmedia-ref/deploy"
  tar -czf "$T/repo.tar.gz" -C "$T/pkg" matrixmedia-ref
  # Fake curl: logs every call, writes the tarball to -o, or fails when told to.
  cat > "$T/bin/curl" <<FAKE
#!/usr/bin/env bash
echo "\$*" >> "$T/curl.log"
[ -z "\${FAKE_CURL_FAIL:-}" ] || exit 22
out=""
while [ \$# -gt 0 ]; do [ "\$1" = "-o" ] && { out="\$2"; shift; }; shift; done
[ -z "\$out" ] || cp "$T/repo.tar.gz" "\$out"
FAKE
  chmod +x "$T/bin/curl"
  : > "$T/curl.log"
}
teardown() { teardown_tmp; rm -rf "$T"; }

# Pipe install.sh into bash from a directory with no deploy/ in it: the curl|bash shape.
piped() {
  run bash -c "cd '$T/empty' && cat '$DEPLOY_ROOT/install.sh' | PATH='$T/bin:$PATH' bash -s -- $* 2>&1"
}

@test "piped with no deploy/ beside it, install.sh downloads the repo and finishes from it" {
  piped --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -eq 0 ]
  [[ "$output" == *"dry-run OK"* ]]
  grep -qF 'https://github.com/MatrixMedia-Project/matrixmedia/archive/main.tar.gz' "$T/curl.log"
}

@test "MM_DEPLOY_REF picks the tag or branch that is downloaded" {
  export MM_DEPLOY_REF=v0.10.1
  piped --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -eq 0 ]
  grep -qF '/archive/v0.10.1.tar.gz' "$T/curl.log"
}

@test "the downloaded tree is kept under MM_ROOT/src (mmctl is a symlink into it)" {
  piped --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -eq 0 ]
  [ -f "$MM_ROOT/src/deploy/install.sh" ]
  [ -f "$MM_ROOT/src/deploy/lib/common.sh" ]
}

@test "a re-run replaces the earlier download instead of mixing old files in" {
  mkdir -p "$MM_ROOT/src/deploy"; : > "$MM_ROOT/src/deploy/STALE"
  piped --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -eq 0 ]
  [ ! -e "$MM_ROOT/src/deploy/STALE" ]
}

@test "a failed download stops with a message naming it, not a sourcing error" {
  export FAKE_CURL_FAIL=1
  piped --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -ne 0 ]
  [[ "$output" == *"could not download"* ]]
  [[ "$output" != *"No such file"* ]]
}

@test "run from a checkout, install.sh downloads nothing" {
  run bash -c "PATH='$T/bin:$PATH' bash '$DEPLOY_ROOT/install.sh' --dry-run --domain ci.example.com --email a@b.co 2>&1"
  [ "$status" -eq 0 ]
  [ ! -s "$T/curl.log" ]
}

# A downloaded tree has no .git, so `mmctl version` printed "dev" on every piped
# install: the operator could not say which release they run.
@test "the piped bootstrap records which ref it downloaded" {
  export MM_DEPLOY_REF=v0.10.1
  piped --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -eq 0 ]
  [ "$(cat "$MM_ROOT/src/.deploy-ref")" = "v0.10.1" ]
}
