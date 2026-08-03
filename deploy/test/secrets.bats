load helper
setup() { setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/secrets.sh"; }
teardown() { teardown_tmp; }

@test "gen_secret writes a hex value of requested length" {
  gen_secret FOO 32
  grep -q '^FOO=[0-9a-f]\{32\}$' "$MM_ROOT/.env.secrets"
}
@test "gen_secret is idempotent — never rotates an existing value" {
  gen_secret FOO 32; local first; first="$(grep '^FOO=' "$MM_ROOT/.env.secrets")"
  gen_secret FOO 32; local second; second="$(grep '^FOO=' "$MM_ROOT/.env.secrets")"
  [ "$first" = "$second" ]
}
@test "generate_secrets produces the full required key set" {
  generate_secrets
  for k in LK_API_KEY LK_API_SECRET MM_AS_TOKEN MM_HS_TOKEN MM_ADMIN_TOKEN \
           MM_JWT_SIGNING_KEY MM_SWITCH_AUTH_SECRET MM_SIGNUP_IP_HASH_PEPPER \
           SYNAPSE_REGISTRATION_SECRET SYNAPSE_MACAROON_SECRET SYNAPSE_FORM_SECRET \
           POSTGRES_SYNAPSE_PASS POSTGRES_APP_ADMIN_PASS POSTGRES_APP_PASS \
           MINIO_ROOT_PASSWORD REDIS_PASSWORD TURN_SECRET TURN_PASS GRAFANA_ADMIN_PASSWORD; do
    grep -q "^${k}=" "$MM_ROOT/.env.secrets" || { echo "missing $k"; return 1; }
  done
}
@test "env.secrets is mode 0600" {
  generate_secrets
  [ "$(stat -f '%Lp' "$MM_ROOT/.env.secrets" 2>/dev/null || stat -c '%a' "$MM_ROOT/.env.secrets")" = "600" ]
}

@test "write_secret_files creates all 4 secret files" {
  generate_secrets
  write_secret_files
  [ -f "$MM_ROOT/secrets/mm_db_app_password" ]
  [ -f "$MM_ROOT/secrets/mm_db_admin_password" ]
  [ -f "$MM_ROOT/secrets/synapse_registration_shared_secret" ]
  [ -f "$MM_ROOT/secrets/signup_ip_hash_pepper" ]
}

@test "write_secret_files secret files have mode 0600" {
  generate_secrets
  write_secret_files
  for name in mm_db_app_password mm_db_admin_password synapse_registration_shared_secret signup_ip_hash_pepper; do
    [ "$(stat -f '%Lp' "$MM_ROOT/secrets/$name" 2>/dev/null || stat -c '%a' "$MM_ROOT/secrets/$name")" = "600" ]
  done
}

@test "write_secret_files secret files have correct content" {
  generate_secrets
  write_secret_files
  app_pass="$(grep '^POSTGRES_APP_PASS=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  admin_pass="$(grep '^POSTGRES_APP_ADMIN_PASS=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  reg_secret="$(grep '^SYNAPSE_REGISTRATION_SECRET=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  pepper="$(grep '^MM_SIGNUP_IP_HASH_PEPPER=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
  [ "$(cat "$MM_ROOT/secrets/mm_db_app_password")" = "$app_pass" ]
  [ "$(cat "$MM_ROOT/secrets/mm_db_admin_password")" = "$admin_pass" ]
  [ "$(cat "$MM_ROOT/secrets/synapse_registration_shared_secret")" = "$reg_secret" ]
  [ "$(cat "$MM_ROOT/secrets/signup_ip_hash_pepper")" = "$pepper" ]
}

@test "write_secret_files secret files have no trailing newline" {
  generate_secrets
  write_secret_files
  for name in mm_db_app_password mm_db_admin_password synapse_registration_shared_secret signup_ip_hash_pepper; do
    f="$MM_ROOT/secrets/$name"
    # File size must equal length of the value (no trailing newline byte)
    val="$(grep "^${name//_db_app_password/POSTGRES_APP_PASS}" "$MM_ROOT/.env.secrets" 2>/dev/null || true)"
    # Check that the last byte is NOT a newline (0x0a)
    last="$(tail -c 1 "$f" | xxd -p 2>/dev/null || tail -c 1 "$f" | od -An -tx1 | tr -d ' \n')"
    [ "$last" != "0a" ] || { echo "trailing newline in $name"; return 1; }
  done
}
