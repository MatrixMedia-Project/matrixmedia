load helper

# The shipped homeserver template used to hard-code `retention: enabled: true` with
# max_lifetime 3d and a purge job every 12h. Every self-hoster who ran the installer was
# therefore silently destroying all message history older than three days — a default they
# never chose and never saw. These tests pin the safe default so it cannot come back.

setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/render.sh"
  mkdir -p "$MM_ROOT/templates" "$MM_ROOT/config"
  cp "$DEPLOY_ROOT/templates/homeserver.tmpl.yaml" "$MM_ROOT/templates/"
  : > "$MM_ROOT/.env.secrets"
}
teardown() { teardown_tmp; }

# Write the .env exactly as install.sh does for a default (non-retention) install.
_env_defaults() {
  cat > "$MM_ROOT/.env" <<EOF
MM_DOMAIN=example.com
MM_RETENTION_ENABLED=false
MM_RETENTION_MIN_LIFETIME=1d
MM_RETENTION_MAX_LIFETIME=90d
EOF
}

@test "a default install does NOT purge message history" {
  _env_defaults
  render_one "$MM_ROOT/templates/homeserver.tmpl.yaml" "$MM_ROOT/config/homeserver.yaml"

  # The load-bearing assertion. `retention.enabled: false` means Synapse deletes nothing,
  # whatever the lifetimes say.
  grep -qE '^\s+enabled: false' "$MM_ROOT/config/homeserver.yaml"

  # And the old landmine value must be gone from the actual config (not from the
  # explanatory comment, which names it deliberately) — so match only real YAML keys.
  ! grep -qE '^\s+(max_lifetime|longest_max_lifetime): 3d\s*$' "$MM_ROOT/config/homeserver.yaml"
}

@test "the retention block is fully substituted (no \${VAR} reaches Synapse)" {
  _env_defaults
  render_one "$MM_ROOT/templates/homeserver.tmpl.yaml" "$MM_ROOT/config/homeserver.yaml"
  # An unsubstituted ${MM_RETENTION_ENABLED} would reach Synapse as a literal string —
  # which YAML happily parses as truthy, i.e. retention silently ON. Assert on the
  # retention vars specifically; the rest of the template needs the full install env, which
  # the real render_templates supplies and this unit-level test does not.
  # (The comment block names the vars deliberately, so match the unsubstituted ${...} form.)
  ! grep -qF '${MM_RETENTION' "$MM_ROOT/config/homeserver.yaml"
}

@test "an operator who opts in gets a coherent policy" {
  cat > "$MM_ROOT/.env" <<EOF
MM_DOMAIN=example.com
MM_RETENTION_ENABLED=true
MM_RETENTION_MIN_LIFETIME=1d
MM_RETENTION_MAX_LIFETIME=30d
EOF
  render_one "$MM_ROOT/templates/homeserver.tmpl.yaml" "$MM_ROOT/config/homeserver.yaml"

  grep -qE '^\s+enabled: true' "$MM_ROOT/config/homeserver.yaml"
  grep -q 'max_lifetime: 30d' "$MM_ROOT/config/homeserver.yaml"

  # purge_jobs.longest_max_lifetime MUST track max_lifetime. If it lags, Synapse runs no
  # purge job for the configured policy and retention silently does nothing — the operator
  # believes they have retention and does not.
  grep -q 'longest_max_lifetime: 30d' "$MM_ROOT/config/homeserver.yaml"
}

@test "install.sh defaults retention to disabled" {
  # Guards the other half: the template can be safe while install.sh writes an unsafe .env.
  grep -q 'MM_RETENTION_ENABLED=${MM_RETENTION_ENABLED:-false}' "$DEPLOY_ROOT/install.sh"
}

@test "an upgrade from an .env with no retention knobs renders safely (does not fail, does not purge)" {
  # An existing install's .env predates the retention knobs entirely. mmctl upgrade
  # re-renders against it. Without a default, ${MM_RETENTION_ENABLED} survives into
  # homeserver.yaml and the upgrade dies on assert_rendered_clean — for a reason the
  # operator cannot act on.
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"

  render_one "$MM_ROOT/templates/homeserver.tmpl.yaml" "$MM_ROOT/config/homeserver.yaml"

  ! grep -qF '${MM_RETENTION' "$MM_ROOT/config/homeserver.yaml"
  # And the value it silently adopts must be the SAFE one.
  grep -qE '^\s+enabled: false' "$MM_ROOT/config/homeserver.yaml"
}
