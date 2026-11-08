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

# install.sh runs under `set -euo pipefail`, where an empty token extraction failed the
# pipeline and exited before the die: the install stopped with no message at all.
@test "capture_admin_token says why under install.sh's set -euo pipefail" {
  printf '#!/usr/bin/env bash\necho "Gateway Timeout"\n' > "$MM_ROOT/bin/curl"
  chmod +x "$MM_ROOT/bin/curl"
  run env MM_ROOT="$MM_ROOT" PATH="$PATH" bash -c 'set -euo pipefail
    source "$1/lib/common.sh"; source "$1/lib/secrets.sh"; source "$1/lib/bootstrap.sh"
    capture_admin_token example.com admin Sekr1tPw' _ "$DEPLOY_ROOT"
  [ "$status" -ne 0 ]
  [[ "$output" == *"could not capture admin token"* ]] || false
  [[ "$output" == *"Gateway Timeout"* ]] || false
  [[ "$output" != *"Sekr1tPw"* ]] || false
}

@test ".env.secrets stays mode 0600 after upsert" {
  capture_admin_token "example.com" "admin" "pw" 2>/dev/null
  [ "$(file_mode "$MM_ROOT/.env.secrets")" = "600" ]
}
