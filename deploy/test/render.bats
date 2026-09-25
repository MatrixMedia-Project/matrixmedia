load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/render.sh"
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  printf 'LK_API_SECRET=abc123\n'  > "$MM_ROOT/.env.secrets"
  mkdir -p "$MM_ROOT/templates" "$MM_ROOT/config"
  cp "$DEPLOY_ROOT/test/fixtures/sample.tmpl.yaml" "$MM_ROOT/templates/"
}
teardown() { teardown_tmp; }

@test "render substitutes set vars" {
  render_one "$MM_ROOT/templates/sample.tmpl.yaml" "$MM_ROOT/config/sample.yaml"
  grep -q 'server_name: example.com' "$MM_ROOT/config/sample.yaml"
  grep -q 'secret: abc123' "$MM_ROOT/config/sample.yaml"
}
@test "render leaves unrelated \${NOT_A_VAR} untouched" {
  render_one "$MM_ROOT/templates/sample.tmpl.yaml" "$MM_ROOT/config/sample.yaml"
  grep -q 'literal ${NOT_A_VAR} stays' "$MM_ROOT/config/sample.yaml"
}
@test "assert_rendered_clean dies on residual \${VAR}" {
  printf 'x: ${STILL_HERE}\n' > "$MM_ROOT/config/bad.yaml"
  run assert_rendered_clean "$MM_ROOT/config/bad.yaml"
  [ "$status" -ne 0 ]
}
@test "assert_rendered_clean passes on a fully-substituted file" {
  printf 'x: example.com\n' > "$MM_ROOT/config/good.yaml"
  run assert_rendered_clean "$MM_ROOT/config/good.yaml"
  [ "$status" -eq 0 ]
}
@test "rendered traefik-dynamic gates mm-switch metrics/health off the internet" {
  render_one "$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml" "$MM_ROOT/config/traefik-dynamic.yaml"
  grep -qF '!Path(`/_mm/switch/metrics`)' "$MM_ROOT/config/traefik-dynamic.yaml"
  grep -qF '!Path(`/_mm/switch/health`)'  "$MM_ROOT/config/traefik-dynamic.yaml"
  # compose label must stay in lockstep with the file-provider rule
  grep -qF '!Path(`/_mm/switch/metrics`)' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
  grep -qF '!Path(`/_mm/switch/health`)'  "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

# FR-349b. GET /api/viewers returns every viewer id on the node, and viewer ids
# are derived from Matrix user ids, so an open endpoint on an internet-facing
# router discloses which Matrix users are watching which stream. The gate is at
# the application layer rather than on the router, because mm-core calls the same
# path WITH a server token and excluding it here would break the operator console.
@test "mm-switch viewer list is gated, and defaults closed when .env says nothing" {
  grep -qF 'MM_SWITCH_PRIVATE_VIEWER_LIST: ${MM_SWITCH_PRIVATE_VIEWER_LIST:-true}' \
    "$DEPLOY_ROOT/docker-compose.tmpl.yml"
  # An operator may flip it, so it must be documented — but the documented value
  # is the closed one.
  grep -qE '^MM_SWITCH_PRIVATE_VIEWER_LIST=true$' "$DEPLOY_ROOT/.env.example"
}

# FR-348. Fleet nodes refuse to boot without an auth secret; the origin may still
# run unsecured for single-host OSS installs. The origin must therefore SAY it is
# the origin, or a future default flip silently changes which one it is.
@test "the compose origin declares its node flavor" {
  grep -qE '^\s+MM_SWITCH_NODE_FLAVOR: origin$' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}
