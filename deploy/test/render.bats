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
