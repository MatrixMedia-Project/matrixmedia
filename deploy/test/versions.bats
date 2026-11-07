load helper

# versions.env pins all 16 images so two installs a week apart run the same stack. It was
# COMPLETELY DEAD: nothing sourced it, nothing passed it to compose. Worse, install.sh
# wrote MM_REGISTRY / MM_VERSION into .env — which compose reads and which therefore WINS
# — naming an org that does not exist (ghcr.io/matrixmedia) and a stale version. These
# tests keep the pins wired.

setup() { setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; }
teardown() { teardown_tmp; }

@test "compose_env_files puts versions.env BEFORE .env (so .env can still override)" {
  : > "$MM_ROOT/versions.env"; : > "$MM_ROOT/.env"; : > "$MM_ROOT/.env.secrets"
  compose_env_files

  # Order is the whole point: docker compose lets a LATER --env-file override an earlier
  # one, so the pins must come first and the operator's config second.
  local joined="${MM_ENV_FILES[*]}"
  [[ "$joined" == *"versions.env"*"/.env"* ]]

  # versions.env must be the FIRST env-file, not somewhere after .env.
  [ "${MM_ENV_FILES[1]}" = "$MM_ROOT/versions.env" ]
}

@test "compose_env_files degrades gracefully when versions.env is absent" {
  # An install predating versions.env has none on disk. It must still start.
  : > "$MM_ROOT/.env"; : > "$MM_ROOT/.env.secrets"
  compose_env_files
  local joined="${MM_ENV_FILES[*]}"
  [[ "$joined" != *"versions.env"* ]]
  [[ "$joined" == *"/.env"* ]]
}

@test "install.sh does NOT write image pins into .env (they would override versions.env)" {
  # The original bug: .env carried MM_REGISTRY=ghcr.io/matrixmedia (a nonexistent org) and
  # MM_VERSION=0.8.1, and .env wins over versions.env — so the pins were unreachable even
  # once wired.
  ! grep -qE '^MM_REGISTRY=\$\{MM_REGISTRY:-' "$DEPLOY_ROOT/install.sh"
  ! grep -qE '^MM_VERSION=\$\{MM_VERSION:-'   "$DEPLOY_ROOT/install.sh"
  # It must ship versions.env to the host, or compose has nothing to read.
  grep -q 'cp "$HERE/versions.env" "$MM_ROOT/"' "$DEPLOY_ROOT/install.sh"
}

@test "versions.env names the registry org that actually exists" {
  # `ghcr.io/matrixmedia` is not the project's org; images there 404 at pull time and the
  # whole install dies after the operator has already pointed DNS at the box.
  grep -q '^MM_REGISTRY=ghcr.io/matrixmedia-project$' "$DEPLOY_ROOT/versions.env"
  ! grep -qE '^MM_REGISTRY=ghcr.io/matrixmedia$' "$DEPLOY_ROOT/versions.env"
}

@test "the documented one-liner points at the real repo path" {
  # The published curl URL named org 'matrixmedia', which does not exist — so the very
  # first command in the README 404s. The repo is MatrixMedia-Project/matrixmedia.
  grep -q 'raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh' \
    "$DEPLOY_ROOT/README.md"
  ! grep -q 'raw.githubusercontent.com/matrixmedia/matrixmedia' "$DEPLOY_ROOT/README.md"
}

@test "every image the compose template pulls is pinned in versions.env" {
  # A service whose image is not pinned silently falls back to the inline default and
  # drifts. Catch a new un-pinned service at review time, not at 'works on my machine'.
  local missing=""
  while read -r var; do
    grep -qE "^${var}=" "$DEPLOY_ROOT/versions.env" || missing="$missing $var"
  done < <(grep -oE '\$\{[A-Z0-9_]+_IMAGE' "$DEPLOY_ROOT/docker-compose.tmpl.yml" \
             | tr -d '${' | sort -u)
  [ -z "$missing" ] || { echo "un-pinned images:$missing"; false; }
}

@test ".env.example must NOT ship image pins (they override versions.env)" {
  # `mmctl init` and the README both tell an operator to write .env FROM this file, and .env
  # WINS over versions.env. Shipping MM_REGISTRY here — as it did, naming an org that does
  # not exist — meant the documented manual path produced a stack that 404s at pull time.
  ! grep -qE '^MM_REGISTRY=' "$DEPLOY_ROOT/.env.example"
  ! grep -qE '^MM_VERSION=' "$DEPLOY_ROOT/.env.example"
  ! grep -qE '^MM_SWITCH_VERSION=' "$DEPLOY_ROOT/.env.example"
}

@test "LNbits image comes from the maintained lnbits/lnbits repo, pinned semver" {
  # lnbitsdocker/lnbits is abandoned (no push since 2021); the maintained repo
  # publishes semver tags. A floating or SHA tag here reintroduces drift.
  grep -qE '^LNBITS_IMAGE=lnbits/lnbits:v[0-9]+\.[0-9]+\.[0-9]+$' "$DEPLOY_ROOT/versions.env"
  ! grep -qE '^LNBITS_IMAGE=.*lnbitsdocker' "$DEPLOY_ROOT/versions.env"
}

@test "compose template sets no LNBITS_ADMIN_USERS (fresh-volume bootstrap owns admin)" {
  # Pre-naming an admin that does not exist yet on a fresh volume locks the
  # operator out of the Admin UI; docs/lnbits.md bootstraps it instead.
  ! grep -q 'LNBITS_ADMIN_USERS' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
  [ -f "$DEPLOY_ROOT/docs/lnbits.md" ]
  grep -q 'Fresh-volume bootstrap' "$DEPLOY_ROOT/docs/lnbits.md"
}

@test "install.sh never hand-rolls an --env-file pair (it would bypass the pins)" {
  # The installer's final act re-created mm-core with a hand-rolled
  # `--env-file .env --env-file .env.secrets`, i.e. WITHOUT versions.env — un-pinning, at
  # the finish line, the one service we most care about pinning.
  ! grep -q -- '--env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets"' "$DEPLOY_ROOT/install.sh"
  grep -q 'compose_env_files' "$DEPLOY_ROOT/install.sh"
}

@test "first-party pins are the release version CI publishes (the Cargo workspace version)" {
  # build-images.yml tags mm-core, mm-switch, mm-web and mm-fakestripe with the release
  # tag, and the release commit sets [workspace.package] version to that tag. Nothing else
  # exists on ghcr: v0.10.0 shipped while these still named 0.9.4 / 0.5.4 / 0.2.1, all 404,
  # so every fresh install died at pull time after the operator had pointed DNS at the box.
  local rel bad=""
  rel="$(awk -F'"' '/^\[workspace.package\]/{w=1} w && /^version *=/{print $2; exit}' "$DEPLOY_ROOT/../Cargo.toml")"
  [ -n "$rel" ]
  for want in "MM_VERSION=$rel" "MM_SWITCH_VERSION=$rel" \
      "MM_WEB_IMAGE=ghcr.io/matrixmedia-project/matrixmedia-mm-web:$rel" \
      "MM_FAKESTRIPE_IMAGE=ghcr.io/matrixmedia-project/matrixmedia-mm-fakestripe:$rel"; do
    grep -qx "$want" "$DEPLOY_ROOT/versions.env" || bad="$bad $want"
  done
  [ -z "$bad" ] || { echo "versions.env should pin:$bad"; false; }
}

@test "the hard-coded first-party fallbacks agree with versions.env" {
  # build.sh and the compose template each repeat a default for when versions.env is not
  # sourced. A release that bumps only versions.env leaves those naming a tag that 404s.
  source "$DEPLOY_ROOT/lib/build.sh"
  local pinned fallback fake
  pinned="$(set -a; source "$DEPLOY_ROOT/versions.env"; firstparty_build_list /src)"
  fallback="$(unset MM_REGISTRY MM_VERSION MM_SWITCH_VERSION MM_FAKESTRIPE_IMAGE; firstparty_build_list /src)"
  [ "$pinned" = "$fallback" ] || { diff <(echo "$pinned") <(echo "$fallback"); false; }
  fake="$(sed -n 's/^MM_FAKESTRIPE_IMAGE=//p' "$DEPLOY_ROOT/versions.env")"
  grep -qF "\${MM_FAKESTRIPE_IMAGE:-$fake}" "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}
