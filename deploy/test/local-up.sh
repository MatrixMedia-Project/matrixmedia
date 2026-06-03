#!/usr/bin/env bash
# Local Docker smoke for the installer's RENDERED CONFIGS + compose wiring.
# NOT the production install path: it skips preflight/DNS/ACME/Traefik-TLS and
# brings up only the config-consuming services on the project network, then
# proves Synapse boots against our homeserver.yaml/appservice/log.config and the
# mm-postgres init.sql created the app DB + roles. mm-core itself is covered by
# its own test suite + `docker compose config`; we don't rebuild it here.
#
# Usage: local-up.sh [up|down]   (default up). Needs a running Docker daemon.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$HERE/.." && pwd)"
PROJECT=mmlocal
MM_ROOT="${MM_ROOT:-/tmp/mm-local}"; export MM_ROOT
# shellcheck disable=SC1091
source "$DEPLOY_DIR/lib/common.sh"
# shellcheck disable=SC1091
source "$DEPLOY_DIR/lib/secrets.sh"
# shellcheck disable=SC1091
source "$DEPLOY_DIR/lib/render.sh"

OVERRIDE="$MM_ROOT/local-override.yml"
# Services whose rendered config we want to exercise (all public images +
# the locally-built mm-switch). Excludes mm-core/mm-web (own images), traefik
# (ACME), coturn (UDP range), lk-jwt/egress/ingress/lnbits (not needed here).
CORE=(postgres synapse mm-postgres lk-redis livekit minio mm-switch)

compose() {
  docker compose --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -f "$OVERRIDE" -p "$PROJECT" "$@"
}

if [ "${1:-up}" = down ]; then
  if [ -f "$OVERRIDE" ]; then compose down -v -t3 || true; fi
  rm -rf "$MM_ROOT"; log "local stack torn down"; exit 0
fi

rm -rf "$MM_ROOT"; mkdir -p "$MM_ROOT/templates" "$MM_ROOT/config"
cp "$DEPLOY_DIR"/templates/*.tmpl.* "$MM_ROOT/templates/"
cp "$DEPLOY_DIR/docker-compose.tmpl.yml" "$MM_ROOT/docker-compose.tmpl.yml"  # render_templates copies → docker-compose.yml
cat > "$MM_ROOT/.env" <<EOF
MM_ROOT=$MM_ROOT
MM_DOMAIN=mm.local
MM_PUBLIC_IP=127.0.0.1
ACME_EMAIL=local@mm.local
MM_REGISTRY=ghcr.io/matrixmedia
MM_VERSION=0.8.1
MM_SWITCH_VERSION=0.5.12
MM_DEMO_MODE=true
MM_ALLOW_MOCK=true
MM_STRIPE_API_BASE=http://mm-fakestripe:8787/
MM_STRIPE_SECRET_KEY=sk_test_local
MM_STRIPE_WEBHOOK_SECRET=whsec_local
EOF
generate_secrets
write_secret_files
render_templates
log "rendered $(find "$MM_ROOT/config" -type f | wc -l | tr -d ' ') config files"

# High host ports to avoid colliding with any other local stack already on the
# default ports (synapse 8008, livekit 7880, etc.).
cat > "$OVERRIDE" <<'YAML'
services:
  synapse:
    ports: ["127.0.0.1:18008:8008"]
  livekit:
    ports: ["127.0.0.1:17880:7880"]
  mm-switch:
    ports: ["127.0.0.1:17890:7890"]
YAML

log "pulling images for: ${CORE[*]}"
compose pull "${CORE[@]}" 2>&1 | grep -iE 'pull|error' | tail -8 || true
log "starting ${CORE[*]}"
compose up -d "${CORE[@]}"

log "waiting for Synapse (our stack) on :18008 (up to 5m)..."
ok=0
for i in $(seq 1 60); do
  sv="$(curl -fsS -o /dev/null -w '%{http_code}' http://127.0.0.1:18008/_matrix/client/versions 2>/dev/null || echo 000)"
  lk="$(curl -fsS -o /dev/null -w '%{http_code}' http://127.0.0.1:17880/ 2>/dev/null || echo 000)"
  echo "  [$i] synapse=$sv livekit=$lk"
  if [ "$sv" = 200 ]; then ok=1; break; fi
  sleep 5
done

echo "--- mm-postgres init.sql result ---"
docker exec matrixmedia-mm-postgres-1 psql -U postgres -tAc \
  "SELECT rolname FROM pg_roles WHERE rolname IN ('mm_app','mm_admin'); SELECT datname FROM pg_database WHERE datname='matrixmedia';" 2>&1 || true
echo "--- container status ---"
compose ps
[ "$ok" -eq 1 ] || die "Synapse did not become ready — check rendered homeserver.yaml/appservice"
log "LOCAL SMOKE PASS — Synapse booted on our rendered configs. Tear down: $0 down"
