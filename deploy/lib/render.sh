# shellcheck shell=bash
: "${MM_ROOT:=/opt/mm}"

# Load env (.env then .env.secrets, secrets win) into the current shell, exported.
_render_load_env() {
  set -a
  # shellcheck disable=SC1091
  [ -f "$MM_ROOT/.env" ] && source "$MM_ROOT/.env"
  # shellcheck disable=SC1091
  [ -f "$MM_ROOT/.env.secrets" ] && source "$MM_ROOT/.env.secrets"
  set +a
}

# Allow-list: the ${VAR}s in the template that are SET in the env. envsubst then
# substitutes ONLY these, leaving any unrelated ${...} (unset vars, literals)
# untouched. `|| true` keeps set -e/pipefail happy when grep finds no match.
_render_varlist() {
  local tmpl="$1" v out=""
  for v in $(grep -oE '\$\{[A-Z0-9_]+\}' "$tmpl" | tr -d '{}$' | sort -u || true); do
    [ -n "${!v:-}" ] && out="$out \${$v}"
  done
  echo "$out"
}

render_one() {
  require_cmd envsubst
  local tmpl="$1" dest="$2"
  _render_load_env
  local list; list="$(_render_varlist "$tmpl")"
  envsubst "$list" < "$tmpl" > "$dest"
}

# Fail loudly if a rendered file still contains an unsubstituted ${VAR}.
assert_rendered_clean() {
  local f="$1" residue
  residue="$(grep -oE '\$\{[A-Z0-9_]+\}' "$f" | sort -u | tr '\n' ' ' || true)"
  [ -z "$residue" ] || die "render: $f has unsubstituted vars: $residue"
}

# Render every templates/*.tmpl.* into config/, asserting each is fully
# substituted. The compose template keeps its ${VAR} refs for `docker compose
# --env-file` to interpolate at up-time, so it is copied, not envsubst'd.
render_templates() {
  mkdir -p "$MM_ROOT/config"
  local t dest
  for t in "$MM_ROOT/templates/"*.tmpl.*; do
    [ -e "$t" ] || continue
    dest="$MM_ROOT/config/$(basename "${t/.tmpl/}")"
    render_one "$t" "$dest"
    assert_rendered_clean "$dest"
  done
  [ -f "$MM_ROOT/docker-compose.tmpl.yml" ] && cp "$MM_ROOT/docker-compose.tmpl.yml" "$MM_ROOT/docker-compose.yml"
}
