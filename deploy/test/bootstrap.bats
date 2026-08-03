load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/secrets.sh"; source "$DEPLOY_ROOT/lib/bootstrap.sh"
  mkdir -p "$MM_ROOT/bin"; PATH="$MM_ROOT/bin:$PATH"
  printf '#!/usr/bin/env bash\necho "{\\"access_token\\":\\"syt_TESTTOKEN\\"}"\n' > "$MM_ROOT/bin/curl"
  chmod +x "$MM_ROOT/bin/curl"
  : > "$MM_ROOT/.env.secrets"
}
teardown() { teardown_tmp; }

@test "capture_admin_token appends MM_SYNAPSE_ADMIN_TOKEN to .env.secrets" {
  capture_admin_token "example.com" "admin" "pw" 2>/dev/null
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_TESTTOKEN$' "$MM_ROOT/.env.secrets"
}

@test "capture_admin_token OVERWRITES an existing placeholder (single line, real token)" {
  printf 'MM_SYNAPSE_ADMIN_TOKEN=deadbeefplaceholder\n' >> "$MM_ROOT/.env.secrets"
  capture_admin_token "example.com" "admin" "pw" 2>/dev/null
  [ "$(grep -c '^MM_SYNAPSE_ADMIN_TOKEN=' "$MM_ROOT/.env.secrets")" -eq 1 ]
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_TESTTOKEN$' "$MM_ROOT/.env.secrets"
  ! grep -q 'deadbeefplaceholder' "$MM_ROOT/.env.secrets"
}

@test "capture_admin_token dies when login returns no token" {
  printf '#!/usr/bin/env bash\necho "{\\"errcode\\":\\"M_FORBIDDEN\\"}"\n' > "$MM_ROOT/bin/curl"
  chmod +x "$MM_ROOT/bin/curl"
  run capture_admin_token "example.com" "admin" "wrongpw"
  [ "$status" -ne 0 ]
}

@test ".env.secrets stays mode 0600 after upsert" {
  capture_admin_token "example.com" "admin" "pw" 2>/dev/null
  [ "$(stat -f '%Lp' "$MM_ROOT/.env.secrets" 2>/dev/null || stat -c '%a' "$MM_ROOT/.env.secrets")" = "600" ]
}
