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
