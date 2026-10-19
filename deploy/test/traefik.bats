load helper

setup() { setup_tmp; }
teardown() { teardown_tmp; }

@test "element-call is file-routed: router + service in traefik-dynamic, docker labels off" {
  dyn="$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml"
  grep -q 'rule: "Host(`call.${MM_DOMAIN}`)"' "$dyn"
  grep -q 'url: http://element-call:8080' "$dyn"
  # The compose service must not ALSO route via the docker provider (two
  # routers for one host = duplicate/conflicting routes).
  block="$(awk '/^  element-call:/,/^  lk-jwt-service:/' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
  [[ "$block" == *"traefik.enable=false"* ]]
  [[ "$block" != *"traefik.http.routers"* ]]
}

@test "lk-jwt is file-routed with the old priority-200 strip rule" {
  dyn="$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml"
  block="$(awk '/^    lk-jwt:/,/^    # ── LNbits/' "$dyn")"
  grep -q 'PathPrefix(`/lk-jwt`)' <<<"$block"
  grep -q 'priority: 200' <<<"$block"
  grep -q 'lk-jwt-strip' <<<"$block"
  grep -q 'prefixes:' "$dyn"
  grep -q 'url: http://lk-jwt-service:8080' "$dyn"
  svc="$(awk '/^  lk-jwt-service:/,/^volumes:/' "$DEPLOY_ROOT/docker-compose.tmpl.yml")"
  [[ "$svc" == *"traefik.enable=false"* ]]
  [[ "$svc" != *"traefik.http.routers"* ]]
  # The Synapse catch-all still excludes /lk-jwt, or the file router fights it.
  grep -q '!PathPrefix(`/lk-jwt`)' "$dyn"
}

@test "compose template has no obsolete version key (compose v5 warns and ignores it)" {
  ! grep -qE '^[[:space:]]*version:[[:space:]]' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

@test "rendered traefik-dynamic.yaml parses and every router's service + middlewares exist" {
  python3 -c "import yaml" 2>/dev/null || skip "pyyaml not installed"
  export MM_DOMAIN=example.com
  # Render only the ${MM_DOMAIN} var the template uses (same allow-list idea as
  # render.sh, without needing envsubst).
  python3 - "$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml" "$MM_ROOT/dynamic.yaml" <<'EOF'
import sys
src, dest = sys.argv[1], sys.argv[2]
text = open(src).read().replace('${MM_DOMAIN}', 'example.com')
assert '${' not in text, "unsubstituted vars remain"
open(dest, 'w').write(text)
EOF
  python3 - "$MM_ROOT/dynamic.yaml" <<'EOF'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
http = d['http']
routers, services, middlewares = http['routers'], http['services'], http['middlewares']
assert {'element-call', 'lk-jwt'} <= set(routers), [r for r in routers if 'call' in r or 'jwt' in r]
for name, r in routers.items():
    assert r['service'] in services, f"router {name} -> missing service {r['service']}"
    for m in r.get('middlewares', []):
        assert m in middlewares, f"router {name} -> missing middleware {m}"
assert services['element-call']['loadBalancer']['servers'][0]['url'] == 'http://element-call:8080'
assert services['lk-jwt-service']['loadBalancer']['servers'][0]['url'] == 'http://lk-jwt-service:8080'
assert routers['lk-jwt'].get('priority') == 200
EOF
}
