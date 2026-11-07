#!/usr/bin/env bash
# mm-allinone -- init, process wrappers and health check of the MatrixMedia all-in-one
# try-it image (infra/docker/Dockerfile.allinone). One file, so the whole lifecycle
# reads top to bottom.
#
#   mm-allinone start        entrypoint: first-start setup, render configs, exec supervisord
#   mm-allinone run NAME     one supervised process: postgres, synapse, livekit, mm-switch,
#                            fakestripe, mm-core, nginx (supervisord.conf calls these)
#   mm-allinone bootstrap    one-shot: create the admin account, hand mm-core its Synapse
#                            admin token, print the "ready" banner
#   mm-allinone health       the image's HEALTHCHECK: Synapse and mm-core both answer
#   mm-allinone credentials  print the admin login, the admin API token and the URLs
#
# TRIAL ONLY. Plain HTTP, secrets generated into the /data volume, demo payments. The
# production stack is deploy/ (one service per container, TLS, real secrets handling).
set -euo pipefail

DATA=/data
SECRETS=$DATA/secrets.env
RUNTIME=/run/matrixmedia                   # configs rendered at every start, never persisted
PG_BIN=/usr/local/lib/pgbin                # symlink to /usr/lib/postgresql/<major>/bin
PG_DATA=$DATA/postgres
SYNAPSE_URL=http://127.0.0.1:8009          # internal; nginx owns the public :8008
ADMIN_URL=http://127.0.0.1:6168            # mm-core admin API, internal
FRONT_URL=http://127.0.0.1:8008

log() { printf '[mm-allinone] %s\n' "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

# Plain shell variables, not exported: each wrapper exports only what its process needs.
# shellcheck source=/dev/null
load_secrets() { . "$SECRETS"; }

# gen BYTES -- hex string of BYTES random bytes. Python is in the base image; openssl is not.
gen() { python3 -c 'import secrets, sys; print(secrets.token_hex(int(sys.argv[1])))' "$1"; }
# gen_key -- 64 URL-safe base64 characters. mm-core warns about a JWT key with few distinct
# bytes, and hex has only 16.
gen_key() { python3 -c 'import secrets; print(secrets.token_urlsafe(48))'; }

# wait_for WHAT SECONDS CMD... -- poll CMD once a second until it succeeds.
wait_for() {
  local what=$1 secs=$2 i
  shift 2
  for ((i = 0; i < secs; i++)); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 1
  done
  die "$what did not come up within ${secs}s"
}

pg_ready() { "$PG_BIN/pg_isready" -q -h 127.0.0.1 -p 5432; }
http_ok() { curl -fsS -o /dev/null --max-time 4 "$@"; }

# Every process's output goes through sed, so `docker logs` says which process wrote each
# line. The program is exec'd after the redirect, so supervisord still signals it directly.
label_output() { exec > >(sed -u "s/^/[$1] /") 2>&1; }

# ---------------------------------------------------------------------------------------
# start (the entrypoint)
# ---------------------------------------------------------------------------------------

# The address browsers, phones and WebRTC use to reach this container. LiveKit and
# mm-switch advertise it as their ICE candidate, so it must be an address the CLIENT can
# reach: the host's LAN IP for phones, not localhost.
resolve_ipv4() {
  if [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf '%s\n' "$1"
  else
    getent ahostsv4 "$1" | awk 'NR == 1 { print $1 }'
  fi
}

# A Matrix server name is permanent: every user id and room in the database carries it.
# It is fixed on the first start and stored in the volume; the public address may change.
ensure_server_name() {
  local f=$DATA/server_name stored
  if [ -s "$f" ]; then
    stored=$(cat "$f")
    if [ -n "${MM_SERVER_NAME:-}" ] && [ "$MM_SERVER_NAME" != "$stored" ]; then
      die "this volume was created with MM_SERVER_NAME=$stored and a Matrix server name cannot change; drop MM_SERVER_NAME or use a new volume"
    fi
    MM_SERVER_NAME=$stored
  else
    : "${MM_SERVER_NAME:=localhost}"
    printf '%s\n' "$MM_SERVER_NAME" >"$f"
  fi
}

ensure_secrets() {
  [ -s "$SECRETS" ] && return 0
  local user=${MM_ADMIN_USER:-admin}
  [[ $user =~ ^[a-z0-9._=-]+$ ]] || die "MM_ADMIN_USER must be a Matrix localpart (a-z 0-9 . _ = -)"
  log "first start: generating secrets into $SECRETS"
  (
    umask 077
    {
      echo "# Generated on the first start. Delete the volume to start over: editing this"
      echo "# file does not change passwords already set in Postgres or Synapse."
      echo "POSTGRES_SYNAPSE_PASS=$(gen 24)"
      echo "POSTGRES_MM_PASS=$(gen 24)"
      echo "SYNAPSE_REGISTRATION_SECRET=$(gen 32)"
      echo "SYNAPSE_MACAROON_SECRET=$(gen 32)"
      echo "SYNAPSE_FORM_SECRET=$(gen 32)"
      echo "MM_AS_TOKEN=$(gen 32)"
      echo "MM_HS_TOKEN=$(gen 32)"
      echo "MM_JWT_SIGNING_KEY=$(gen_key)"
      echo "MM_ADMIN_TOKEN=$(gen 32)"
      echo "MM_ALERT_WEBHOOK_TOKEN=$(gen 32)"
      echo "MM_SETTINGS_ENCRYPTION_KEY=$(gen 32)"
      echo "MM_SWITCH_AUTH_SECRET=$(gen 32)"
      echo "MM_SIGNUP_IP_HASH_PEPPER=$(gen 32)"
      echo "MM_STRIPE_WEBHOOK_SECRET=whsec_$(gen 24)"
      echo "LK_API_KEY=API$(gen 6)"
      echo "LK_API_SECRET=$(gen 32)"
      echo "MM_ADMIN_USER=$user"
      printf 'MM_ADMIN_PASSWORD=%q\n' "${MM_ADMIN_PASSWORD:-$(gen 9)}"
    } >"$SECRETS.partial"
  )
  mv "$SECRETS.partial" "$SECRETS"
}

# One cluster, two databases: synapse and matrixmedia, each owned by its own role,
# reachable on 127.0.0.1 only. The marker is written last, so a first start interrupted
# half-way (nothing has used the cluster yet) is redone instead of trusted.
ensure_postgres() {
  install -d -o postgres -g postgres -m 0755 /run/postgresql
  [ -f "$DATA/.postgres-initialized" ] && return 0
  log "first start: creating the Postgres cluster"
  rm -rf "$PG_DATA"
  install -d -o postgres -g postgres -m 0700 "$PG_DATA"
  gosu postgres "$PG_BIN/initdb" -D "$PG_DATA" -U postgres --auth-local=peer \
    --auth-host=scram-sha-256 --encoding=UTF8 --locale=C >/dev/null
  gosu postgres "$PG_BIN/pg_ctl" -D "$PG_DATA" -w -s \
    -o "-c listen_addresses='' -c unix_socket_directories=/run/postgresql" start
  # Passwords go in on stdin, never on a command line.
  gosu postgres "$PG_BIN/psql" -q -v ON_ERROR_STOP=1 -h /run/postgresql -U postgres -d postgres <<SQL
CREATE ROLE synapse LOGIN PASSWORD '$POSTGRES_SYNAPSE_PASS';
CREATE DATABASE synapse OWNER synapse ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;
CREATE ROLE mm_admin LOGIN PASSWORD '$POSTGRES_MM_PASS';
CREATE DATABASE matrixmedia OWNER mm_admin ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;
SQL
  gosu postgres "$PG_BIN/pg_ctl" -D "$PG_DATA" -w -s stop
  touch "$DATA/.postgres-initialized"
}

render_synapse() {
  local dir=$RUNTIME/synapse
  install -d -o root -g synapse -m 0750 "$dir"
  install -d -o synapse -g synapse -m 0750 "$DATA/synapse" "$DATA/synapse/media_store"
  (
    umask 027
    cat >"$dir/homeserver.yaml" <<YAML
# Rendered by mm-allinone at every start; edits here are lost.
server_name: "$MM_SERVER_NAME"
public_baseurl: "$MM_PUBLIC_URL/"
pid_file: $DATA/synapse/homeserver.pid
listeners:
  - port: 8009
    bind_addresses: ["127.0.0.1"]
    type: http
    tls: false
    x_forwarded: true
    resources:
      - names: [client]
        compress: false
database:
  name: psycopg2
  args:
    user: synapse
    password: "$POSTGRES_SYNAPSE_PASS"
    database: synapse
    host: 127.0.0.1
    port: 5432
    cp_min: 2
    cp_max: 10
log_config: $dir/log.config
media_store_path: $DATA/synapse/media_store
signing_key_path: $DATA/synapse/signing.key
registration_shared_secret: "$SYNAPSE_REGISTRATION_SECRET"
macaroon_secret_key: "$SYNAPSE_MACAROON_SECRET"
form_secret: "$SYNAPSE_FORM_SECRET"
report_stats: false
# Accounts come from mm-core's signup (it registers users with the shared secret) or
# from the admin. Open Synapse registration stays off.
enable_registration: false
app_service_config_files:
  - $dir/mm_appservice.yaml
# A local trial does not federate: no federation listener, no key servers.
federation_domain_whitelist: []
trusted_key_servers: []
suppress_key_server_warning: true
url_preview_enabled: false
max_upload_size: 50M
user_directory:
  enabled: true
  search_all_users: true
  prefer_local_users: true
room_list_publication_rules:
  - user_id: "*"
    alias: "*"
    room_id: "*"
    action: allow
rc_message:
  per_second: 10
  burst_count: 50
YAML
    cat >"$dir/log.config" <<'YAML'
version: 1
formatters:
  precise:
    format: '%(name)s - %(levelname)s - %(request)s - %(message)s'
handlers:
  console:
    class: logging.StreamHandler
    formatter: precise
root:
  level: WARNING
  handlers: [console]
loggers:
  # Startup and "now listening" lines; the per-request access log stays quiet.
  synapse.app:
    level: INFO
disable_existing_loggers: false
YAML
    cat >"$dir/mm_appservice.yaml" <<YAML
id: matrixmedia
url: "http://127.0.0.1:6167"
as_token: "$MM_AS_TOKEN"
hs_token: "$MM_HS_TOKEN"
sender_localpart: mmbot
namespaces:
  users:
    - exclusive: false
      regex: "@mmbot:.*"
  rooms: []
  aliases: []
rate_limited: false
protocols: []
YAML
  )
  chgrp synapse "$dir"/*
  rm -f "$DATA/synapse/homeserver.pid"
  if [ ! -s "$DATA/synapse/signing.key" ]; then
    log "first start: generating the Synapse signing key"
    gosu synapse python -m synapse.app.homeserver -c "$dir/homeserver.yaml" --generate-keys >/dev/null
  fi
}

render_livekit() {
  install -d -o root -g mm -m 0750 "$RUNTIME/livekit"
  (
    umask 027
    cat >"$RUNTIME/livekit/livekit.yaml" <<YAML
# Rendered by mm-allinone at every start; edits here are lost.
port: 7880
rtc:
  tcp_port: 7881
  # One UDP port for all media (LiveKit's UDP mux), so one -p flag publishes it.
  udp_port: 7882
  use_external_ip: false
  node_ip: $MM_NODE_IP
turn:
  enabled: false
keys:
  $LK_API_KEY: $LK_API_SECRET
webhook:
  api_key: $LK_API_KEY
  urls:
    - http://127.0.0.1:6167/_mm/internal/v1/sfu/webhook
room:
  auto_create: true
  empty_timeout: 300
logging:
  level: info
YAML
  )
  chgrp mm "$RUNTIME/livekit/livekit.yaml"
}

cmd_start() {
  : "${MM_PUBLIC_HOST:=localhost}"
  : "${MM_PUBLIC_URL:=http://$MM_PUBLIC_HOST:8008}"
  MM_PUBLIC_URL=${MM_PUBLIC_URL%/}
  : "${MM_DEMO_MODE:=true}"
  case $MM_DEMO_MODE in true | false) ;; *) die "MM_DEMO_MODE must be true or false" ;; esac
  MM_NODE_IP=$(resolve_ipv4 "$MM_PUBLIC_HOST")
  [ -n "$MM_NODE_IP" ] || die "cannot resolve MM_PUBLIC_HOST=$MM_PUBLIC_HOST to an IPv4 address"

  install -d -m 0755 "$DATA" "$RUNTIME"
  ensure_server_name
  ensure_secrets
  load_secrets
  # Nothing runs yet, so a postmaster.pid is left over from a container that was killed.
  # Its PID may belong to an unrelated process in this new PID namespace, and Postgres
  # would then refuse to start.
  rm -f "$PG_DATA/postmaster.pid"
  ensure_postgres
  install -d -o mm -g mm -m 0755 "$DATA/mm-core" "$DATA/recordings"
  render_synapse
  render_livekit

  # Non-secret settings every process inherits through supervisord, also kept in a file
  # for `docker exec … mm-allinone credentials`, which does not inherit them. Secrets stay
  # in the 0600 file; each wrapper reads the ones its process needs.
  export MM_PUBLIC_HOST MM_PUBLIC_URL MM_NODE_IP MM_DEMO_MODE MM_SERVER_NAME
  local v
  for v in MM_PUBLIC_HOST MM_PUBLIC_URL MM_NODE_IP MM_DEMO_MODE MM_SERVER_NAME; do
    printf '%s=%q\n' "$v" "${!v}"
  done >"$RUNTIME/runtime.env"
  log "public URL $MM_PUBLIC_URL, media address $MM_NODE_IP, server name $MM_SERVER_NAME, demo payments $MM_DEMO_MODE"
  [[ $MM_NODE_IP == 127.* ]] &&
    log "MM_PUBLIC_HOST is a loopback address: only this machine can connect. For phones and other devices, set -e MM_PUBLIC_HOST=<this host's LAN IP>"
  exec supervisord -n -c /etc/matrixmedia/supervisord.conf
}

# ---------------------------------------------------------------------------------------
# run NAME (one supervised process each)
# ---------------------------------------------------------------------------------------

run_postgres() {
  exec gosu postgres "$PG_BIN/postgres" -D "$PG_DATA" \
    -c listen_addresses=127.0.0.1 -c unix_socket_directories=/run/postgresql
}

run_synapse() {
  wait_for Postgres 120 pg_ready
  exec gosu synapse python -m synapse.app.homeserver -c "$RUNTIME/synapse/homeserver.yaml"
}

run_livekit() {
  exec gosu mm livekit-server --config "$RUNTIME/livekit/livekit.yaml"
}

run_mm_switch() {
  load_secrets
  export MM_SWITCH_LISTEN=127.0.0.1:7890 \
    MM_SWITCH_PUBLIC_IP=$MM_NODE_IP \
    MM_SWITCH_UDP_MODE=mux \
    MM_SWITCH_UDP_PORT=50100 \
    MM_SWITCH_AUTH_SECRET \
    MM_SWITCH_NODE_FLAVOR=origin \
    MM_SWITCH_PRIVATE_VIEWER_LIST=true
  exec gosu mm mm-switch
}

run_fakestripe() {
  load_secrets
  export MM_FAKESTRIPE_LISTEN=127.0.0.1:8787 \
    MM_FAKESTRIPE_WEBHOOK_URL=http://127.0.0.1:6167/_mm/webhooks/stripe \
    MM_FAKESTRIPE_WEBHOOK_SECRET=$MM_STRIPE_WEBHOOK_SECRET \
    MM_FAKESTRIPE_WEBHOOK_DELAY_MS=500 \
    RUST_LOG=mm_fakestripe=info
  exec gosu mm mm-fakestripe
}

run_mm_core() {
  load_secrets
  # mm-core reads MM_SYNAPSE_ADMIN_TOKEN from its environment only, so it waits for
  # bootstrap to create the admin account and write the token.
  if [ -z "${MM_SYNAPSE_ADMIN_TOKEN:-}" ]; then
    log "waiting for bootstrap to create the admin account"
    wait_for "the admin token" 600 grep -q '^MM_SYNAPSE_ADMIN_TOKEN=.' "$SECRETS"
    load_secrets
  fi
  # On a restart the token is already there; without this wait mm-core would come up before
  # Synapse and log a burst of failed homeserver calls.
  wait_for Synapse 300 http_ok "$SYNAPSE_URL/_matrix/client/versions"

  # Payments: with demo mode on, mm-core talks to mm-fakestripe (in this container) and
  # never to Stripe; off, monetization is off. No real Stripe key can reach this image.
  local monetization=$MM_DEMO_MODE
  export MM_MATRIX_HOMESERVER_URL=$SYNAPSE_URL \
    MM_MATRIX_PUBLIC_HOMESERVER_URL=$MM_PUBLIC_URL \
    MM_MATRIX_SERVER_NAME=$MM_SERVER_NAME \
    MM_MATRIX_BOT_LOCALPART=mmbot \
    MM_MATRIX_AS_TOKEN=$MM_AS_TOKEN \
    MM_MATRIX_HS_TOKEN=$MM_HS_TOKEN \
    MM_SYNAPSE_ADMIN_TOKEN \
    MM_SYNAPSE_REGISTRATION_SHARED_SECRET=$SYNAPSE_REGISTRATION_SECRET \
    MM_DATABASE_URL=postgres://mm_admin:$POSTGRES_MM_PASS@127.0.0.1:5432/matrixmedia \
    MM_POSTGRES_URL=postgres://mm_admin:$POSTGRES_MM_PASS@127.0.0.1:5432/matrixmedia \
    MM_JWT_SIGNING_KEY MM_ADMIN_TOKEN MM_ALERT_WEBHOOK_TOKEN MM_SETTINGS_ENCRYPTION_KEY \
    MM_SERVER_PUBLIC_URL=$MM_PUBLIC_URL \
    MM_CORS_ORIGINS=$MM_PUBLIC_URL \
    MM_SFU_LIVEKIT_URL=http://127.0.0.1:7880 \
    MM_SFU_LIVEKIT_PUBLIC_URL=${MM_PUBLIC_URL/#http/ws}/livekit \
    MM_SFU_LIVEKIT_API_KEY=$LK_API_KEY \
    MM_SFU_LIVEKIT_API_SECRET=$LK_API_SECRET \
    MM_SWITCH_URL=http://127.0.0.1:7890 \
    MM_SWITCH_LEGACY_LK_SOURCE=false \
    MM_SWITCH_AUTH_SECRET \
    MM_SIGNUP_IP_HASH_PEPPER \
    MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR=30 \
    MM_SIGNUP_TOS_CURRENT_VERSION=v1 \
    MM_DEMO_MODE \
    MM_MONETIZATION_ENABLED=$monetization \
    MM_MONETIZATION_DONATIONS_ENABLED=$monetization \
    MM_MONETIZATION_SUBSCRIPTIONS_ENABLED=$monetization \
    MM_STRIPE_SECRET_KEY=sk_test_allinone_fakestripe \
    MM_STRIPE_WEBHOOK_SECRET \
    MM_STRIPE_API_BASE=http://127.0.0.1:8787/ \
    MM_ALLOW_MOCK=true \
    MM_FEDERATION_ENABLED=false \
    MM_FEED_ENABLED=true \
    MM_ADVERTISING_ENABLED=false \
    MM_LNBITS_ENABLED=false \
    MM_FLEET_MODE=frozen \
    MM_STORAGE_BACKEND=local \
    MM_STORAGE_LOCAL_PATH=$DATA/mm-core/media \
    RUST_LOG=${RUST_LOG:-info}
  # mm-core resolves some defaults against its working directory (media: "data/media").
  # From / that is /data/media, which the mm user cannot create.
  cd "$DATA/mm-core"
  exec gosu mm matrixmedia serve --bind 127.0.0.1:6167 --admin-bind 127.0.0.1:6168 \
    --data-dir "$DATA/mm-core"
}

run_nginx() {
  exec nginx -c /etc/matrixmedia/nginx.conf -g 'daemon off;'
}

cmd_run() {
  local name=${1:?usage: mm-allinone run NAME}
  label_output "$name"
  case $name in
    postgres) run_postgres ;;
    synapse) run_synapse ;;
    livekit) run_livekit ;;
    mm-switch) run_mm_switch ;;
    fakestripe) run_fakestripe ;;
    mm-core) run_mm_core ;;
    nginx) run_nginx ;;
    *) die "unknown process: $name" ;;
  esac
}

# ---------------------------------------------------------------------------------------
# bootstrap, health, credentials
# ---------------------------------------------------------------------------------------

# synapse_login -- log the admin in and print the access token. Python builds the JSON, so
# a password with quotes or backslashes cannot break it.
synapse_login() {
  MM_ADMIN_USER=$MM_ADMIN_USER MM_ADMIN_PASSWORD=$MM_ADMIN_PASSWORD python3 - "$SYNAPSE_URL" <<'PY'
import json, os, sys, urllib.request
body = json.dumps({
    "type": "m.login.password",
    "identifier": {"type": "m.id.user", "user": os.environ["MM_ADMIN_USER"]},
    "password": os.environ["MM_ADMIN_PASSWORD"],
    "device_id": "MM_CORE_SERVICE",
    "initial_device_display_name": "mm-core (all-in-one)",
}).encode()
req = urllib.request.Request(sys.argv[1] + "/_matrix/client/v3/login", data=body,
                             headers={"Content-Type": "application/json"})
print(json.load(urllib.request.urlopen(req, timeout=10))["access_token"])
PY
}

cmd_bootstrap() {
  label_output bootstrap
  load_secrets
  if [ -z "${MM_SYNAPSE_ADMIN_TOKEN:-}" ]; then
    wait_for Synapse 300 http_ok "$SYNAPSE_URL/_matrix/client/versions"
    log "creating the admin account @$MM_ADMIN_USER:$MM_SERVER_NAME"
    # The shared secret comes from the config file and the password from a pipe, so
    # neither shows up in `ps`.
    register_new_matrix_user --exists-ok --admin -u "$MM_ADMIN_USER" \
      --password-file <(printf '%s' "$MM_ADMIN_PASSWORD") \
      -c "$RUNTIME/synapse/homeserver.yaml" "$SYNAPSE_URL" >/dev/null
    local token
    token=$(synapse_login) || die "admin login failed"
    printf 'MM_SYNAPSE_ADMIN_TOKEN=%s\n' "$token" >>"$SECRETS"
  fi
  wait_for mm-core 300 http_ok -H "Authorization: Bearer $MM_ADMIN_TOKEN" "$ADMIN_URL/_mm/admin/v1/health"
  cat <<EOF

  ================================================================================
   MatrixMedia is ready  (trial image, development branch, commit ${MM_BUILD_COMMIT:-unknown})

     Dashboard     $MM_PUBLIC_URL/_mm/dashboard/
     Viewer        $MM_PUBLIC_URL/_mm/viewer/
     Homeserver    $MM_PUBLIC_URL   (server name: $MM_SERVER_NAME)
     Admin login   $MM_ADMIN_USER / $MM_ADMIN_PASSWORD
     Admin API     docker exec <container> mm-allinone credentials

   Demo payments are $( [ "$MM_DEMO_MODE" = true ] && echo "ON: fake Stripe inside this container, no real money moves" || echo "OFF: monetization is disabled" ).
   For trying MatrixMedia only. Production installs: deploy/README.md.
  ================================================================================

EOF
}

# Each process directly, plus nginx's own /health. Probing the backends through nginx would
# log a "connection refused" error for every check made while they are still starting.
cmd_health() {
  http_ok "$SYNAPSE_URL/_matrix/client/versions"
  load_secrets
  http_ok -H "Authorization: Bearer $MM_ADMIN_TOKEN" "$ADMIN_URL/_mm/admin/v1/health"
  http_ok "$FRONT_URL/health"
}

cmd_credentials() {
  load_secrets
  # shellcheck source=/dev/null
  . "$RUNTIME/runtime.env"
  cat <<EOF
Dashboard        $MM_PUBLIC_URL/_mm/dashboard/
Homeserver       $MM_PUBLIC_URL   (server name: $MM_SERVER_NAME)
Admin user       @$MM_ADMIN_USER:$MM_SERVER_NAME
Admin password   $MM_ADMIN_PASSWORD
Admin API token  $MM_ADMIN_TOKEN
  e.g. curl -H "Authorization: Bearer \$TOKEN" $MM_PUBLIC_URL/_mm/admin/v1/health
EOF
}

case ${1:-start} in
  start) cmd_start ;;
  run) shift; cmd_run "$@" ;;
  bootstrap) cmd_bootstrap ;;
  health) cmd_health ;;
  credentials) cmd_credentials ;;
  *) die "usage: mm-allinone start|run NAME|bootstrap|health|credentials" ;;
esac
