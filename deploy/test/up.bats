load helper
setup() { setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/up.sh"; }
teardown() {
  docker compose -f "$DEPLOY_ROOT/test/fixtures/healthy-compose.yml" -p mmtest down -t1 >/dev/null 2>&1 || true
  teardown_tmp
}

@test "wait_healthy returns 0 when the fixture service becomes healthy" {
  command -v docker >/dev/null || skip "docker not available"
  docker info >/dev/null 2>&1 || skip "docker daemon not running"
  docker compose -f "$DEPLOY_ROOT/test/fixtures/healthy-compose.yml" -p mmtest up -d
  run wait_healthy mmtest 60
  [ "$status" -eq 0 ]
}
