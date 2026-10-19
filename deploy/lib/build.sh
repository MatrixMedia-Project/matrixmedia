# shellcheck shell=bash
# Native first-party image builds for the cutover NEW box (decision B1).
#
# The NEW host builds ALL first-party images natively from the development
# branch checkout — never save/load-transferred as tarballs from OLD (a tarball
# skips provenance, rots across architectures, and cannot be re-audited; a
# native `docker build` from a pinned branch sha can). Third-party images are
# NOT built: they pull at the newest-stable pins in versions.env.
: "${MM_ROOT:=/opt/mm}"

# firstparty_build_list SRC -- print one `tag|dockerfile|context|target` row per
# first-party image (empty target = build the Dockerfile's last stage). Mirrors
# the matrix in .github/workflows/build-images.yml — if CI adds an image, add
# its row here too. Pure: no docker calls, so bats pins the contract without
# building anything. Tags follow $MM_ROOT/versions.env when sourced, else the
# current pins as defaults.
firstparty_build_list() {
  local src="$1"
  local reg="${MM_REGISTRY:-ghcr.io/matrixmedia-project}"
  local ver="${MM_VERSION:-0.9.4}" sw="${MM_SWITCH_VERSION:-0.5.4}"
  local fake="${MM_FAKESTRIPE_IMAGE:-$reg/matrixmedia-mm-fakestripe:0.2.1}"
  printf '%s|%s|%s|%s\n' \
    "$reg/matrixmedia-mm-core:$ver"         "$src/infra/docker/Dockerfile"           "$src"                 "mm-core" \
    "$reg/matrixmedia-mm-media-worker:$ver" "$src/infra/docker/Dockerfile"           "$src"                 "media-worker" \
    "$reg/matrixmedia-mm-switch:$sw"        "$src/services/mm-switch/Dockerfile"     "$src/services/mm-switch" "" \
    "$reg/matrixmedia-mm-web:$ver"          "$src/infra/docker/Dockerfile.web"       "$src"                 "" \
    "$fake"                                 "$src/infra/docker/Dockerfile.fakestripe" "$src"                ""
}

# build_firstparty [SRC_ROOT] -- docker-build every first-party image from the
# development checkout at SRC_ROOT (default: the repo root above deploy/).
# Tags match versions.env, so the rendered compose uses the local builds with
# no registry round-trip. Prints the built tags + the source sha for the
# cutover version record (deploy/docs/native-build.md).
build_firstparty() {
  local src="${1:-}" tag df ctx target commit
  [ -n "$src" ] || src="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  [ -d "$src/infra/docker" ] || die "--build needs the repo checkout (clone the development branch and run deploy/install.sh --build from it); got SRC=$src"
  require_cmd docker
  # Pins live in versions.env, which install.sh copies to $MM_ROOT before
  # building; sourcing it keeps tags identical to what compose resolves.
  # shellcheck disable=SC1091
  [ -f "$MM_ROOT/versions.env" ] && source "$MM_ROOT/versions.env"
  commit="$(git -C "$src" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  log "building first-party images from $src (commit $commit)"
  firstparty_build_list "$src" | while IFS='|' read -r tag df ctx target; do
    [ -f "$df" ] || die "missing Dockerfile $df (checkout incomplete?)"
    [ -d "$ctx" ] || die "missing build context $ctx"
    if [ -n "$target" ]; then
      docker build -f "$df" --target "$target" -t "$tag" \
        --build-arg "MM_BUILD_COMMIT=$commit" "$ctx" || die "build failed: $tag"
    else
      docker build -f "$df" -t "$tag" \
        --build-arg "MM_BUILD_COMMIT=$commit" "$ctx" || die "build failed: $tag"
    fi
    log "built $tag"
  done
  log "native build complete from $commit; record: $(firstparty_build_list "$src" | cut -d'|' -f1 | tr '\n' ' ')"
}
