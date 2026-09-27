load helper
# The mm-web nginx template: every directory-style /_mm/<name>/ location must also
# answer the bare /_mm/<name> (a 301 to the slash form) — otherwise typing the path
# without the slash is a 404 — and redirects must stay relative, because behind
# Traefik's TLS nginx sees plain http and would send clients to http://.

TEMPLATE="$DEPLOY_ROOT/templates/nginx.tmpl.conf"

@test "every /_mm/<name>/ location has a bare-path redirect" {
  missing=""
  for name in $(sed -n 's|^    location /_mm/\([a-z0-9_-]*\)/ {.*|\1|p' "$TEMPLATE"); do
    grep -qF "location = /_mm/${name} { return 301 /_mm/${name}/; }" "$TEMPLATE" \
      || missing="$missing $name"
  done
  [ -z "$missing" ] || { echo "no bare-path redirect for:$missing"; return 1; }
}

@test "the template finds the SPA locations it checks (guard is not vacuous)" {
  n="$(grep -c '^    location /_mm/[a-z0-9_-]*/ {' "$TEMPLATE")"
  [ "$n" -ge 10 ]
}

@test "redirects are relative (absolute_redirect off)" {
  grep -q '^    absolute_redirect off;$' "$TEMPLATE"
}
