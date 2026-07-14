load helper
setup() { setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/preflight.sh"; source "$DEPLOY_ROOT/lib/smoke.sh"; }
teardown() { teardown_tmp; }

# ── smoke: the reason a broken UI could ship ─────────────────────────────────

@test "body_looks_like_spa accepts a real app shell" {
  run body_looks_like_spa '<!doctype html><html><body><div id="root"></div><script type="module" src="/assets/index-abc.js"></script></body></html>'
  [ "$status" -eq 0 ]
}

@test "body_looks_like_spa REJECTS an nginx 404 body" {
  # This is the exact failure that shipped: nginx served an empty html root, every SPA
  # 404'd, and smoke — which only checked status codes, and never probed the SPAs at all —
  # passed and called the install a success.
  run body_looks_like_spa '<html><head><title>404 Not Found</title></head><body><center><h1>404 Not Found</h1></center></body></html>'
  [ "$status" -ne 0 ]
}

@test "body_looks_like_spa REJECTS an empty body served with 200" {
  run body_looks_like_spa ''
  [ "$status" -ne 0 ]
}

@test "body_looks_like_spa REJECTS a directory listing" {
  run body_looks_like_spa '<html><head><title>Index of /</title></head><body><h1>Index of /</h1><hr></body></html>'
  [ "$status" -ne 0 ]
}

@test "code_is_gated: mm-switch internals must not be public" {
  # 200 on /metrics leaks stream + viewer topology and hands over a free liveness oracle.
  run code_is_gated 200; [ "$status" -ne 0 ]
  run code_is_gated 403; [ "$status" -eq 0 ]
  run code_is_gated 404; [ "$status" -eq 0 ]
  run code_is_gated 401; [ "$status" -eq 0 ]
}

@test "federation_report_ok reads the tester verdict" {
  run federation_report_ok '{"WellKnownResult":{},"FederationOK":true}'
  [ "$status" -eq 0 ]
  run federation_report_ok '{"FederationOK":false,"Error":"no address found"}'
  [ "$status" -ne 0 ]
}

# ── preflight ────────────────────────────────────────────────────────────────

@test "clock_skew_secs is symmetric and absolute" {
  [ "$(clock_skew_secs 1000 940)" -eq 60 ]
  [ "$(clock_skew_secs 940 1000)" -eq 60 ]
  [ "$(clock_skew_secs 1000 1000)" -eq 0 ]
}

@test "udp_range_valid rejects the ranges that silently kill all media" {
  # A malformed or blocked media range is the classic self-host failure: calls and streams
  # connect, HTTP health stays green, and no audio or video ever arrives.
  run udp_range_valid 50000 50200; [ "$status" -eq 0 ]
  run udp_range_valid 50200 50000; [ "$status" -ne 0 ]   # inverted
  run udp_range_valid 80 50200;    [ "$status" -ne 0 ]   # privileged
  run udp_range_valid 50000 70000; [ "$status" -ne 0 ]   # past 65535
  run udp_range_valid 50000 50000; [ "$status" -ne 0 ]   # empty
  run udp_range_valid abc 50200;   [ "$status" -ne 0 ]   # garbage
  run udp_range_valid '' '';       [ "$status" -ne 0 ]
}

@test "aaaa_is_a_trap fires only when DNS advertises v6 the host cannot serve" {
  # DNS says AAAA, host has no v6 → v6-preferring clients reach nothing, while the operator
  # (testing over v4) sees a working server.
  run aaaa_is_a_trap yes no;  [ "$status" -eq 0 ]
  run aaaa_is_a_trap yes yes; [ "$status" -ne 0 ]
  run aaaa_is_a_trap no  no;  [ "$status" -ne 0 ]
  run aaaa_is_a_trap no  yes; [ "$status" -ne 0 ]
}

@test "preflight converges on re-run instead of dying on its own Traefik" {
  # The old check died on 'port 80 already in use' — which is true, and it is OUR Traefik.
  # So the installer could be run exactly once, ever, and never again to repair a
  # half-finished install.
  grep -q 'port_is_ours' "$DEPLOY_ROOT/lib/preflight.sh"
  grep -q 'converging' "$DEPLOY_ROOT/lib/preflight.sh"
  ! grep -q 'die "port \$p already in use"$' "$DEPLOY_ROOT/lib/preflight.sh"
}

@test "self_smoke actually probes the web UI and the switch gate" {
  # Guards the wiring, not just the helpers: a perfect probe_spa that nothing calls is
  # exactly the state we were in.
  grep -q 'probe_spa "https://matrix.$domain/_mm/dashboard/"' "$DEPLOY_ROOT/lib/smoke.sh"
  grep -q 'probe_spa "https://matrix.$domain/_mm/viewer/"'    "$DEPLOY_ROOT/lib/smoke.sh"
  grep -q 'probe_gated "https://matrix.$domain/_mm/switch/metrics"' "$DEPLOY_ROOT/lib/smoke.sh"
}
