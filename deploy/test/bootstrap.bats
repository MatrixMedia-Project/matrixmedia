load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/secrets.sh"; source "$DEPLOY_ROOT/lib/bootstrap.sh"
  mkdir -p "$MM_ROOT/bin"; PATH="$MM_ROOT/bin:$PATH"
  printf '#!/usr/bin/env bash\necho "{\\"access_token\\":\\"syt_TESTTOKEN\\"}"\n' > "$MM_ROOT/bin/curl"
  chmod +x "$MM_ROOT/bin/curl"
  : > "$MM_ROOT/.env.secrets"
}
teardown() { [ -z "${FAKE_PID:-}" ] || kill "$FAKE_PID" 2>/dev/null || true; teardown_tmp; }

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

# ── mm-core's own Synapse account ────────────────────────────────────────────
# mm-core's admin token used to be a session of the OWNER's account. A Matrix password
# change logs out the user's other devices by default (logout_devices), so an owner who
# changed the generated password, as the guides say to, revoked mm-core's token: the
# Operator Console then signed them in as "demo", membership gates failed closed, and every
# re-run died on "could not capture admin token". mm-core now has its own admin account.
#
# Fake Synapse. whoami: the token in the -K stdin config is valid if it equals
# FAKE_VALID_TOKEN, and belongs to FAKE_WHOAMI. login (JSON body on stdin): mm-core signs in
# unless FAKE_SERVICE_LOGIN_OK=0; anyone else unless FAKE_OWNER_LOGIN_OK=0.
# Every call's argv and stdin are logged.
_fake_synapse() {
  cat > "$MM_ROOT/bin/curl" <<'FAKE'
#!/usr/bin/env bash
in=""
case " $* " in *" -K - "*|*" -d @- "*) in="$(cat)" ;; esac
echo "ARGV $*" >> "$MM_ROOT/curl.log"
[ -z "$in" ] || echo "STDIN $in" >> "$MM_ROOT/curl.log"
case "$*" in
  *account/whoami*)
    case "$in" in
      *"Bearer ${FAKE_VALID_TOKEN:-none}"*) echo "{\"user_id\":\"${FAKE_WHOAMI:-@mm-core:example.com}\"}" ;;
      *) echo '{"errcode":"M_UNKNOWN_TOKEN","error":"Invalid access token"}' ;;
    esac ;;
  */logout*) echo '{}' ;;
  *login*)
    case "$in" in
      *'"user":"mm-core"'*) if [ "${FAKE_SERVICE_LOGIN_OK:-1}" = 1 ]; then echo '{"access_token":"syt_SERVICE"}'
                            else echo '{"errcode":"M_FORBIDDEN","error":"Invalid username or password"}'; fi ;;
      *) if [ "${FAKE_OWNER_LOGIN_OK:-1}" = 1 ]; then echo '{"access_token":"syt_OWNERCHECK"}'
         else echo '{"errcode":"M_FORBIDDEN","error":"Invalid username or password"}'; fi ;;
    esac ;;
esac
FAKE
  chmod +x "$MM_ROOT/bin/curl"; : > "$MM_ROOT/curl.log"
  docker() {
    local argv="$*"
    echo "DOCKER ${argv//$'\n'/\\n}" >> "$MM_ROOT/docker.log"
    if [ "$2" = -i ]; then echo "DSTDIN $(cat)" >> "$MM_ROOT/docker.log"; fi
  }
  NONINT=1; ADMIN_USER=""; ADMIN_PASS=""
}
# _registered USER -- the docker log shows USER registered in Synapse's container.
_registered() { grep -q "^DOCKER exec -i matrixmedia-synapse-1 python3 -c .* $1 /data/homeserver.yaml http://localhost:8008\$" "$MM_ROOT/docker.log"; }
# A completed install: owner alice (credentials file), mm-core's account and token stored.
_completed_install() {
  printf 'owner_user=alice\nowner_pass=(the password you chose)\n' > "$MM_ROOT/admin.credentials"
  printf 'MM_SYNAPSE_ADMIN_TOKEN=syt_STORED\nMM_SYNAPSE_SERVICE_PASS=svcpw\nMM_OWNER_BOOTSTRAP_PASS=firstpw\n' > "$MM_ROOT/.env.secrets"
}

@test "admin_token_owner names the account a working token signs in as" {
  _fake_synapse; export FAKE_VALID_TOKEN=syt_STORED FAKE_WHOAMI=@alice:example.com
  run admin_token_owner example.com syt_STORED
  [ "$status" -eq 0 ]
  [ "$output" = "@alice:example.com" ]
}
@test "admin_token_owner fails for a token Synapse no longer accepts, or none" {
  _fake_synapse
  run admin_token_owner example.com syt_STORED; [ "$status" -ne 0 ]
  run admin_token_owner example.com "";         [ "$status" -ne 0 ]
}
@test "tokens and passwords never reach curl's command line (ps shows it to every user)" {
  _fake_synapse; export FAKE_VALID_TOKEN=syt_STORED
  admin_token_owner example.com syt_STORED >/dev/null
  capture_admin_token example.com mm-core Sekr1tPw 2>/dev/null
  run grep '^ARGV' "$MM_ROOT/curl.log"
  [[ "$output" != *"syt_STORED"* ]] || false
  [[ "$output" != *"Sekr1tPw"* ]] || false
  grep -q '^STDIN .*Authorization: Bearer syt_STORED' "$MM_ROOT/curl.log"
  grep -q '^STDIN .*"password":"Sekr1tPw"' "$MM_ROOT/curl.log"
}
@test "capture_admin_token sends a password with quotes and backslashes as valid JSON" {
  _fake_synapse
  capture_admin_token example.com alice 'a"b\c' 2>/dev/null
  grep -qF '"password":"a\"b\\c"' "$MM_ROOT/curl.log"
}

@test "a first install creates mm-core's account with a generated password, and uses its token" {
  _fake_synapse
  bootstrap_owner example.com 2>/dev/null
  pass="$(read_secret MM_SYNAPSE_SERVICE_PASS)"
  [ "${#pass}" -ge 32 ]
  _registered mm-core
  grep -qx "DSTDIN $pass" "$MM_ROOT/docker.log"
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_SERVICE$' "$MM_ROOT/.env.secrets"
}
@test "a first install also creates the owner account, checks it can sign in, and signs that session out" {
  _fake_synapse
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 0 ] && [ "$GENERATED_PASS" -eq 1 ] && [ "$ADMIN_USER" = admin ]
  owner_pass="$(read_secret MM_OWNER_BOOTSTRAP_PASS)"; [ -n "$owner_pass" ]
  _registered admin
  grep -qx "DSTDIN $owner_pass" "$MM_ROOT/docker.log"
  grep -q '^STDIN .*"user":"admin"' "$MM_ROOT/curl.log"
  grep -q 'logout' "$MM_ROOT/curl.log"
  grep -q '^STDIN .*Bearer syt_OWNERCHECK' "$MM_ROOT/curl.log"     # the check's session, signed out
  ! grep -q 'syt_OWNERCHECK' "$MM_ROOT/.env.secrets"               # never mm-core's token
}
@test "a first install stops, saying why, when the owner account cannot sign in" {
  _fake_synapse; export FAKE_OWNER_LOGIN_OK=0
  ADMIN_USER=alice; ADMIN_PASS=pw
  run bootstrap_owner example.com
  [ "$status" -ne 0 ]
  [[ "$output" == *"@alice:example.com"* ]] || false
  [[ "$output" == *"Invalid username or password"* ]] || false
}

@test "a re-run keeps mm-core's token while it works: no registration, no sign-in" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=syt_STORED
  bootstrap_owner example.com 2>/dev/null
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_STORED$' "$MM_ROOT/.env.secrets"
  ! grep -q 'login' "$MM_ROOT/curl.log"
  [ ! -s "$MM_ROOT/docker.log" ]
}
# The scenario the guides had a workaround for: the owner changed their password.
@test "a re-run needs nothing from the owner: their account and password are left alone" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=syt_STORED FAKE_OWNER_LOGIN_OK=0
  run bootstrap_owner example.com
  [ "$status" -eq 0 ]
  [[ "$output" != *"could not capture admin token"* ]] || false
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 1 ] && [ "$ADMIN_USER" = alice ]
  ! grep -q '"user":"alice"' "$MM_ROOT/curl.log"
}
@test "a re-run whose token was revoked signs mm-core's account in again with its stored password" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=none
  bootstrap_owner example.com 2>/dev/null
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_SERVICE$' "$MM_ROOT/.env.secrets"
  grep -q '^STDIN .*"user":"mm-core","password":"svcpw"' "$MM_ROOT/curl.log"
  [ "$(read_secret MM_SYNAPSE_SERVICE_PASS)" = svcpw ]            # not regenerated
}
@test "an install whose token was the owner's session moves to mm-core's account" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=syt_STORED FAKE_WHOAMI=@alice:example.com
  bootstrap_owner example.com 2>/dev/null
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_SERVICE$' "$MM_ROOT/.env.secrets"
  _registered mm-core
  [ "$MM_RETIRED_ADMIN_TOKEN" = syt_STORED ]
}
# The running mm-core still holds the old token until install.sh recreates it. Signing it
# out at once left mm-core on a revoked token whenever anything in between failed.
@test "the owner's old session is signed out only when retire_old_admin_token runs" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=syt_STORED FAKE_WHOAMI=@alice:example.com
  bootstrap_owner example.com 2>/dev/null
  ! grep -q 'Bearer syt_STORED.*' <(grep -A1 'logout' "$MM_ROOT/curl.log")
  run retire_old_admin_token example.com
  [ "$status" -eq 0 ]
  [[ "$output" == *"@alice:example.com"* ]] || false
  grep -A1 'logout' "$MM_ROOT/curl.log" | grep -q 'Bearer syt_STORED'
}
@test "retire_old_admin_token does nothing when no token was replaced" {
  _fake_synapse; MM_RETIRED_ADMIN_TOKEN=""
  run retire_old_admin_token example.com
  [ "$status" -eq 0 ]
  ! grep -q logout "$MM_ROOT/curl.log"
}
@test "install.sh signs the old session out only after the recreated mm-core is healthy" {
  healthy="$(grep -n '^wait_healthy matrixmedia 120 ' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  retire="$(grep -n '^retire_old_admin_token "\$DOMAIN"$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  [ -n "$healthy" ] && [ -n "$retire" ]
  [ "$healthy" -lt "$retire" ]
}
@test "no password reaches docker's command line" {
  _fake_synapse
  ADMIN_USER=alice; ADMIN_PASS=Sekr1tOwner
  bootstrap_owner example.com 2>/dev/null
  svc="$(read_secret MM_SYNAPSE_SERVICE_PASS)"
  run grep '^DOCKER' "$MM_ROOT/docker.log"
  [[ "$output" != *"$svc"* ]] || false
  [[ "$output" != *"Sekr1tOwner"* ]] || false
}
@test "a re-run that cannot sign mm-core's account in says so" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=none FAKE_SERVICE_LOGIN_OK=0
  run bootstrap_owner example.com
  [ "$status" -ne 0 ]
  [[ "$output" == *"could not capture admin token"* ]] || false
  [[ "$output" == *"@mm-core:example.com"* ]] || false
  [[ "$output" != *"svcpw"* ]] || false
}
@test "an explicit --admin-user on a re-run provisions that account" {
  _fake_synapse; _completed_install; export FAKE_VALID_TOKEN=syt_STORED
  ADMIN_USER=bob; ADMIN_PASS=bobpw
  bootstrap_owner example.com 2>/dev/null
  [ "$OWNER_REUSED" -eq 0 ]
  _registered bob
  grep -qx 'DSTDIN bobpw' "$MM_ROOT/docker.log"
  grep -q '^MM_SYNAPSE_ADMIN_TOKEN=syt_STORED$' "$MM_ROOT/.env.secrets"   # mm-core's token untouched
}
@test "install.sh provisions through bootstrap_owner, and keeps admin.credentials on a re-run" {
  grep -q '^bootstrap_owner "\$DOMAIN"$' "$DEPLOY_ROOT/install.sh"
  ! grep -q 'register_new_matrix_user' "$DEPLOY_ROOT/install.sh"
  grep -q '^if \[ "\$OWNER_REUSED" -eq 1 \] && \[ -f "\$MM_ROOT/admin.credentials" \]; then' "$DEPLOY_ROOT/install.sh"
}

# ── Registration leaves no session behind ────────────────────────────────────
# Synapse's shared-secret registration also signs the new account in (a device and access
# token) unless the request sets inhibit_login. register_new_matrix_user never sets it and
# throws the token away, so every install left an unused session on the owner account, and
# one on mm-core's.
#
# _fake_register_synapse [EXISTING_USER] -- a fake Synapse (fixtures/fake_synapse_register.py)
# that checks the shared-secret MAC. The docker stub runs the in-container command here,
# with the container's config path and listener mapped to the fake's.
_fake_register_synapse() {
  printf 'server_name: example.com\nregistration_shared_secret: %s\n' \
    "$(printf '0123456789abcdef%.0s' 1 2 3 4)" > "$MM_ROOT/homeserver.yaml"
  python3 "$DEPLOY_ROOT/test/fixtures/fake_synapse_register.py" "$MM_ROOT/homeserver.yaml" \
    "$MM_ROOT/port" "$MM_ROOT/register.log" "${1:-}" >/dev/null 2>&1 3>&- &
  FAKE_PID=$!; disown "$FAKE_PID" 2>/dev/null || true    # no "Terminated" when teardown kills it
  local i; for i in $(seq 50); do [ -s "$MM_ROOT/port" ] && break; sleep 0.1; done
  FAKE_URL="http://127.0.0.1:$(cat "$MM_ROOT/port")"
  : > "$MM_ROOT/docker.log"
  docker() {
    local argv="$*" a args=()
    echo "DOCKER ${argv//$'\n'/\\n}" >> "$MM_ROOT/docker.log"
    [ "$1 $2 $3" = "exec -i matrixmedia-synapse-1" ] || return 1
    shift 3
    for a in "$@"; do
      case "$a" in
        /data/homeserver.yaml) a="$MM_ROOT/homeserver.yaml" ;;
        http://localhost:8008) a="$FAKE_URL" ;;
      esac
      args+=("$a")
    done
    "${args[@]}"
  }
}

@test "registering an account creates it as a Synapse admin and leaves no session behind" {
  _fake_register_synapse
  run _register_user alice pw
  [ "$status" -eq 0 ]
  grep -qx 'REGISTERED alice admin=True device=no' "$MM_ROOT/register.log"
}
@test "registration sends the password exactly, and only on stdin" {
  _fake_register_synapse
  _register_user alice 'a"b\c é $x' 2>/dev/null
  grep -qxF 'PASSWORD a"b\c é $x' "$MM_ROOT/register.log"
  ! grep -qF 'a"b\c' "$MM_ROOT/docker.log"
}
@test "registering an account that exists fails with Synapse's answer" {
  _fake_register_synapse alice
  run _register_user alice pw
  [ "$status" -ne 0 ]
  [[ "$output" == *"User ID already taken"* ]] || false
  [ ! -s "$MM_ROOT/register.log" ]
}
@test "registration fails, saying why and without a traceback, when Synapse does not answer" {
  _fake_register_synapse; kill "$FAKE_PID"
  while kill -0 "$FAKE_PID" 2>/dev/null; do sleep 0.05; done; FAKE_PID=""
  run _register_user alice pw
  [ "$status" -ne 0 ]
  [[ "$output" == *"Synapse"* ]] || false
  [[ "$output" != *"Traceback"* ]] || false
}
@test "a first install leaves no session on either account it creates" {
  _fake_synapse; _fake_register_synapse      # curl from the first, docker from the second
  bootstrap_owner example.com 2>/dev/null
  grep -qx 'REGISTERED mm-core admin=True device=no' "$MM_ROOT/register.log"
  grep -qx 'REGISTERED admin admin=True device=no' "$MM_ROOT/register.log"
}
