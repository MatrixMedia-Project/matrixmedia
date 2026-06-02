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
