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

# ── re-runs: keep a token that still works ───────────────────────────────────
# A re-run used to sign in again with the generated password from the first install. Once
# the owner had changed it (as the guides tell them to), every re-run died with
# "could not capture admin token", although the stored token still worked.
#
# Fake Synapse: whoami answers for FAKE_TOKEN_OK=1, login succeeds unless FAKE_LOGIN_OK=0.
# Every call's argv (and a -K stdin config) is logged.
_fake_synapse() {
  cat > "$MM_ROOT/bin/curl" <<'FAKE'
#!/usr/bin/env bash
echo "ARGV $*" >> "$MM_ROOT/curl.log"
for a in "$@"; do if [ "$a" = "-K" ]; then echo "STDIN $(cat)" >> "$MM_ROOT/curl.log"; break; fi; done
case "$*" in
  *account/whoami*) if [ "${FAKE_TOKEN_OK:-0}" = 1 ]; then echo '{"user_id":"@alice:example.com"}'
                    else echo '{"errcode":"M_UNKNOWN_TOKEN","error":"Invalid access token"}'; fi ;;
  *login*) if [ "${FAKE_LOGIN_OK:-1}" = 1 ]; then echo '{"access_token":"syt_NEWTOKEN"}'
           else echo '{"errcode":"M_FORBIDDEN","error":"Invalid username or password"}'; fi ;;
esac
FAKE
  chmod +x "$MM_ROOT/bin/curl"; : > "$MM_ROOT/curl.log"
  docker() { echo "DOCKER $*" >> "$MM_ROOT/docker.log"; }
  NONINT=1; ADMIN_USER=""; ADMIN_PASS=""
  printf 'MM_SYNAPSE_ADMIN_TOKEN=syt_STORED\nMM_OWNER_BOOTSTRAP_PASS=firstpw\n' > "$MM_ROOT/.env.secrets"
}

@test "admin_token_owner names the account a working token signs in as" {
  _fake_synapse; export FAKE_TOKEN_OK=1
  run admin_token_owner example.com syt_STORED
  [ "$status" -eq 0 ]
  [ "$output" = "@alice:example.com" ]
}
@test "admin_token_owner fails for a token Synapse no longer accepts, or none" {
  _fake_synapse; export FAKE_TOKEN_OK=0
  run admin_token_owner example.com syt_STORED; [ "$status" -ne 0 ]
  run admin_token_owner example.com "";         [ "$status" -ne 0 ]
}
@test "admin_token_owner keeps the token off curl's command line (ps shows it to every user)" {
  _fake_synapse; export FAKE_TOKEN_OK=1
  admin_token_owner example.com syt_STORED >/dev/null
  run grep '^ARGV' "$MM_ROOT/curl.log"
  [[ "$output" != *"syt_STORED"* ]] || false
  grep -q '^STDIN .*Authorization: Bearer syt_STORED' "$MM_ROOT/curl.log"
}

@test "a re-run whose stored token still works keeps it: no registration, no sign-in" {
  _fake_synapse; export FAKE_TOKEN_OK=1 FAKE_LOGIN_OK=0   # the owner changed the password
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 1 ]
  [ "$ADMIN_USER" = alice ]
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_STORED$' "$MM_ROOT/.env.secrets"
  ! grep -q 'login' "$MM_ROOT/curl.log"
  [ ! -s "$MM_ROOT/docker.log" ]
}
@test "the changed-password re-run no longer dies on 'could not capture admin token'" {
  _fake_synapse; export FAKE_TOKEN_OK=1 FAKE_LOGIN_OK=0
  run bootstrap_owner example.com
  [ "$status" -eq 0 ]
  [[ "$output" != *"could not capture admin token"* ]] || false
  [[ "$output" == *"@alice:example.com"* ]] || false
}
@test "a stale token makes the re-run register and sign in, as a first install does" {
  _fake_synapse; export FAKE_TOKEN_OK=0
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 0 ]
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_NEWTOKEN$' "$MM_ROOT/.env.secrets"
  grep -q 'register_new_matrix_user' "$MM_ROOT/docker.log"
  grep -q 'firstpw' "$MM_ROOT/docker.log"           # the generated password, reused
  [ "$GENERATED_PASS" -eq 1 ]
}
@test "an explicit --admin-user naming another account is not overruled by the stored token" {
  _fake_synapse; export FAKE_TOKEN_OK=1
  ADMIN_USER=bob; ADMIN_PASS=bobpw
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 0 ]
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_NEWTOKEN$' "$MM_ROOT/.env.secrets"
}
@test "a first install (no stored token) generates the owner password and signs in" {
  _fake_synapse; : > "$MM_ROOT/.env.secrets"
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 0 ] && [ "$GENERATED_PASS" -eq 1 ] && [ "$ADMIN_USER" = admin ]
  grep -q '^MM_OWNER_BOOTSTRAP_PASS=' "$MM_ROOT/.env.secrets"
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_NEWTOKEN$' "$MM_ROOT/.env.secrets"
}
@test "install.sh provisions the owner through bootstrap_owner, and keeps admin.credentials when it reused the token" {
  grep -q '^bootstrap_owner "\$DOMAIN"$' "$DEPLOY_ROOT/install.sh"
  ! grep -q 'register_new_matrix_user' "$DEPLOY_ROOT/install.sh"
  grep -q '^if \[ "\$OWNER_REUSED" -eq 1 \] && \[ -f "\$MM_ROOT/admin.credentials" \]; then' "$DEPLOY_ROOT/install.sh"
}
