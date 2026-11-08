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

# ── Synapse's admin API stays off the internet ───────────────────────────────
# Synapse's reverse-proxy guide: proxy /_matrix and /_synapse/client, do not expose
# /_synapse/admin. mm-core and the installer reach the admin API on the internal network
# (http://synapse:8008, or docker exec into the container), never through Traefik.

# _route HOST PATH -- print "SERVICE ROUTER" for the router Traefik would pick for an HTTPS
# request to HOST/PATH, or NONE (Traefik then answers 404). It weighs EVERY public router:
# the file routers in traefik-dynamic.tmpl.yaml and the docker-label routers in
# docker-compose.tmpl.yml, because Traefik runs both providers and a path one rule excludes
# still leaks through the other. Highest priority wins; the default priority is the rule's
# length. Rule syntax it does not know fails the test instead of guessing.
_route() {
  python3 - "$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml" "$DEPLOY_ROOT/docker-compose.tmpl.yml" "$1" "$2" <<'EOF'
import re, sys, yaml
dyn, compose, host, path = sys.argv[1:5]

def load(p):
    return yaml.safe_load(open(p).read().replace('${MM_DOMAIN}', 'example.com'))

routers = []  # (name, rule, priority, service)
for name, r in load(dyn)['http']['routers'].items():
    routers.append((name + '@file', r['rule'], r.get('priority'), r['service']))
for svc_name, svc in (load(compose).get('services') or {}).items():
    raw = svc.get('labels') or []
    pairs = raw.items() if isinstance(raw, dict) else (l.split('=', 1) for l in raw)
    labels = {k: str(v) for k, v in pairs}
    if labels.get('traefik.enable') != 'true':
        continue
    declared = {k.split('.')[3] for k in labels if k.startswith('traefik.http.services.')}
    for key, rule in labels.items():
        m = re.fullmatch(r'traefik\.http\.routers\.([^.]+)\.rule', key)
        if not m:
            continue
        n = m.group(1)
        prio = labels.get(f'traefik.http.routers.{n}.priority')
        # No explicit service: Traefik uses the container's one declared service.
        default = next(iter(declared)) if len(declared) == 1 else svc_name
        service = labels.get(f'traefik.http.routers.{n}.service', default)
        routers.append((n + '@docker', rule, int(prio) if prio else None, service))

TOKEN = re.compile(r'\s+|&&|\|\||!|\(|\)|(Host|PathPrefix|Path)\(`([^`]*)`\)')
def to_py(rule):
    out, pos = [], 0
    while pos < len(rule):
        m = TOKEN.match(rule, pos)
        if not m:
            sys.exit(f'unsupported rule syntax at {rule[pos:]!r} in {rule!r}')
        kind, arg, tok = m.group(1), m.group(2), m.group(0)
        pos = m.end()
        if kind == 'Host':
            out.append(f'(host == {arg!r})')
        elif kind == 'Path':
            out.append(f'(path == {arg!r})')
        elif kind == 'PathPrefix':
            out.append(f'path.startswith({arg!r})')
        else:
            out.append({'&&': ' and ', '||': ' or ', '!': ' not '}.get(tok, tok))
    return ''.join(out)

hits = [(prio if prio is not None else len(rule), name, service)
        for name, rule, prio, service in routers
        if eval(to_py(rule), {}, {'host': host, 'path': path})]
if not hits:
    print('NONE')
else:
    _, name, service = max(hits)
    print(service, name)
EOF
}

@test "Synapse's admin API is not routed from the internet (file and docker-label routers)" {
  python3 -c "import yaml" 2>/dev/null || skip "pyyaml not installed"
  for p in /_synapse/admin /_synapse/admin/ /_synapse/admin/v1/server_version \
           /_synapse/admin/v2/users /_synapse/admin/v1/register; do
    run _route matrix.example.com "$p"
    [ "$status" -eq 0 ]
    [[ "$output" == "NONE" ]] || { echo "$p -> $output (must reach no router)"; false; }
  done
}

@test "the Synapse paths clients use still reach Synapse" {
  python3 -c "import yaml" 2>/dev/null || skip "pyyaml not installed"
  # /_synapse/client carries SSO callbacks, password-reset and consent pages.
  for p in /_matrix/client/versions /_matrix/federation/v1/version \
           /_matrix/media/v3/download/example.com/abc /_matrix/key/v2/server \
           /_synapse/client/password_reset/email/submit_token /_synapse/client/oidc/callback \
           /.well-known/matrix/client; do
    run _route matrix.example.com "$p"
    [ "$status" -eq 0 ]
    [[ "$output" == "matrix-synapse "* ]] || { echo "$p -> $output (expected Synapse)"; false; }
  done
  # The evaluator itself routes: mm-core's paths land on mm-core, not on NONE.
  run _route matrix.example.com /_mm/client/v1/rooms
  [[ "$output" == "mm-client "* ]] || false
}

@test "the Synapse catch-all excludes /_synapse/admin in the file AND label rule, kept identical" {
  dyn="$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml"
  file_rule="$(awk '/^    matrix-synapse:/{f=1} f && /rule:/{print; exit}' "$dyn" | sed -E 's/^ *rule: "(.*)"$/\1/')"
  label_rule="$(grep -o 'traefik\.http\.routers\.matrix-synapse\.rule=[^"]*' "$DEPLOY_ROOT/docker-compose.tmpl.yml" | cut -d= -f2-)"
  [[ "$file_rule" == *'!PathPrefix(`/_synapse/admin`)'* ]] || false
  [ "$file_rule" = "$label_rule" ]
}

@test "helm: the Synapse ingress allow-lists client paths, no catch-all on /" {
  ing="$DEPLOY_ROOT/helm/synapse/templates/ingress.yaml"
  # Ingress has no "not this prefix", so the chart routes only what Synapse documents.
  # `! cmd` alone never fails a bats test mid-way (errexit ignores negation): hence || false.
  ! grep -qE '^[[:space:]]*-[[:space:]]*path:[[:space:]]*/[[:space:]]*$' "$ing" || false
  grep -q '"/_matrix"' "$ing"
  grep -q '"/_synapse/client"' "$ing"
  grep -q '"/.well-known/matrix"' "$ing"
  # Outside comments, nothing in the template names the admin API.
  run bash -c "grep -v '^[[:space:]]*#' '$ing' | grep '_synapse/admin'"
  [ "$status" -eq 1 ]
}

@test "helm: rendered Synapse ingress serves exactly /_matrix, /_synapse/client, /.well-known/matrix" {
  command -v helm >/dev/null || skip "helm not installed"
  python3 -c "import yaml" 2>/dev/null || skip "pyyaml not installed"
  helm template t "$DEPLOY_ROOT/helm/synapse" --set ingress.host=matrix.example.com \
    --show-only templates/ingress.yaml > "$MM_ROOT/ing.yaml"
  python3 - "$MM_ROOT/ing.yaml" <<'EOF'
import sys, yaml
ing = yaml.safe_load(open(sys.argv[1]))
rule, = ing['spec']['rules']
assert rule['host'] == 'matrix.example.com', rule['host']
paths = rule['http']['paths']
got = sorted(p['path'] for p in paths)
assert got == ['/.well-known/matrix', '/_matrix', '/_synapse/client'], got
for p in paths:
    assert p['pathType'] == 'Prefix', p
    assert p['backend']['service']['port']['name'] == 'http', p
    assert p['backend']['service']['name'] == 't-synapse', p['backend']
EOF
}

@test "self_smoke fails an install whose Synapse admin API answers from the internet" {
  # server_version needs no token, so a 200 there proves the route is open; probe_gated
  # accepts only 401/403/404. Guards the wiring: a gate nothing probes regresses silently.
  grep -q 'probe_gated "https://matrix.$domain/_synapse/admin/v1/server_version" || fail=1' "$DEPLOY_ROOT/lib/smoke.sh"
}
