# Re-attach regression: a manual `compose up -d <svc>` drops the container from the
# external edge network (compose does not manage it), and until something
# re-attaches it every Traefik route to that backend 502s. The deploy workflow
# re-attaches via deploy/lib/edge-attach.sh; this pins the helper + its wiring.
load helper

setup() {
  setup_tmp
  source "$DEPLOY_ROOT/lib/common.sh"
  # Stub docker: inspect reports $STUB_NETWORKS; connect appends to the call log.
  STUB_BIN="$(mktemp -d)"
  export STUB_BIN DOCKER_CALL_LOG="$BATS_TEST_TMPDIR/calls"
  cat >"$STUB_BIN/docker" <<'STUB'
#!/usr/bin/env bash
if [ "$1" = "inspect" ]; then
  echo "{\"${STUB_NETWORKS// /\":\"\\\"}\": {}}"
  exit 0
fi
if [ "$1" = "network" ]; then
  echo "connect $*" >>"$DOCKER_CALL_LOG"
  [ "${STUB_CONNECT_FAIL:-0}" = "1" ] && exit 1
  exit 0
fi
exit 1
STUB
  chmod +x "$STUB_BIN/docker"
  PATH="$STUB_BIN:$PATH"
}

teardown() { teardown_tmp; rm -rf "$STUB_BIN"; }

@test "edge-attach helper exists and is wired into the stage workflow" {
  [ -f "$DEPLOY_ROOT/lib/edge-attach.sh" ]
  local workflow="$DEPLOY_ROOT/../.github/workflows/deploy-stage.yml"
  [ -f "$workflow" ]
  grep -q "edge-attach.sh" "$workflow"
  grep -q "edge_attach" "$workflow"
  # sudo strips the environment: the lib path must cross sudo via env, exactly
  # like the adjacent roll step. A bare `sudo bash -c 'source $GITHUB_...'` sees
  # an empty workspace and the re-attach silently never runs (stage 502s).
  grep -q 'sudo -n env "MM_LIB=' "$workflow"
}

@test "edge_attach connects backends missing from the edge network" {
  source "$DEPLOY_ROOT/lib/edge-attach.sh"
  STUB_NETWORKS="matrixmedia_mm_network" run edge_attach mm-web mm-core
  [ "$status" -eq 0 ]
  grep -q "network connect shared_network matrixmedia-mm-web-1" "$DOCKER_CALL_LOG"
  grep -q "network connect shared_network matrixmedia-mm-core-1" "$DOCKER_CALL_LOG"
}

@test "edge_attach skips backends already on the edge network" {
  source "$DEPLOY_ROOT/lib/edge-attach.sh"
  STUB_NETWORKS="matrixmedia_mm_network shared_network" run edge_attach mm-web
  [ "$status" -eq 0 ]
  [ ! -f "$DOCKER_CALL_LOG" ]
}

@test "edge_attach fails when the container is gone" {
  source "$DEPLOY_ROOT/lib/edge-attach.sh"
  STUB_NETWORKS="matrixmedia_mm_network" STUB_CONNECT_FAIL=1 run edge_attach mm-web
  [ "$status" -ne 0 ]
}

@test "self_smoke probes the public landing root" {
  # matrix / 500'd (missing landing/ mount) while every other probe passed.
  grep -q 'probe_http "https://matrix.$domain/" 200' "$DEPLOY_ROOT/lib/smoke.sh"
}
