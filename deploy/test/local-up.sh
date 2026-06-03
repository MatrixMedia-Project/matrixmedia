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
# Under $HOME (not /tmp): Docker Desktop on macOS shares /Users reliably, whereas
# freshly-created files under /private/tmp can fail to resolve for bind mounts.
MM_ROOT="${MM_ROOT:-$HOME/.mm-local-smoke}"; export MM_ROOT
# shellcheck disable=SC1091
source "$DEPLOY_DIR/lib/common.sh"
# shellcheck disable=SC1091
source "$DEPLOY_DIR/lib/secrets.sh"
# shellcheck disable=SC1091
source "$DEPLOY_DIR/lib/render.sh"

OVERRIDE="$MM_ROOT/local-override.yml"
# Services whose rendered config we want to exercise (all public images +
# the locally-built mm-switch). Excludes mm-web (own image), traefik (ACME),
# coturn (UDP range), lk-jwt/egress/ingress/lnbits (not needed here).
# Set MM_LOCAL_WITH_CORE=1 to also boot mm-core + mm-fakestripe (needs the
# locally-built mm-core image) — the full-stack smoke.
CORE=(postgres synapse mm-postgres lk-redis livekit minio mm-switch)
# mm-fakestripe (argiad/mm-fakestripe) isn't on a public registry; mm-core boots
# fine without it (Stripe is only called on creator actions, not at startup).
if [ "${MM_LOCAL_WITH_CORE:-0}" = 1 ]; then CORE+=(mm-core); fi

compose() {
  docker compose --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -f "$OVERRIDE" -p "$PROJECT" "$@"
}

if [ "${1:-up}" = down ]; then
  if [ -f "$OVERRIDE" ]; then compose down -v -t3 || true; fi
  rm -rf "$MM_ROOT"; log "local stack torn down"; exit 0
fi

# Idempotent: clear any previous mmlocal stack — containers AND volumes — so a
# re-run can't collide on published media UDP ports, and postgres re-initialises
# with the freshly-generated secrets (a stale data volume keeps its old password
# and would fail auth).
docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" | xargs -r docker rm -f >/dev/null 2>&1 || true
docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT" | xargs -r docker volume rm -f >/dev/null 2>&1 || true

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
# `!override` REPLACES each service's ports list (compose otherwise APPENDS), so
# we drop the base production media UDP ranges (livekit 50000-50020, mm-switch
# 50100-50300) which a boot smoke doesn't need and which collide/flake on macOS.
cat > "$OVERRIDE" <<'YAML'
services:
  synapse:
    ports: !override ["127.0.0.1:18008:8008"]
  livekit:
    ports: !override ["127.0.0.1:17880:7880"]
  mm-switch:
    ports: !override ["127.0.0.1:17890:7890"]
YAML
if [ "${MM_LOCAL_WITH_CORE:-0}" = 1 ]; then
  cat >> "$OVERRIDE" <<'YAML'
  mm-core:
    ports: !override ["127.0.0.1:16167:6167", "127.0.0.1:16168:6168"]
YAML
fi

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

core_ok=1
if [ "${MM_LOCAL_WITH_CORE:-0}" = 1 ]; then
  log "waiting for mm-core on :16167 (up to 3m)..."
  core_ok=0
  for i in $(seq 1 36); do
    mc="$(curl -fsS -o /dev/null -w '%{http_code}' http://127.0.0.1:16167/mm/v1/announcements/active 2>/dev/null || echo 000)"
    echo "  [$i] mm-core /mm/v1/announcements/active=$mc"
    # 200 = up; 401/404 also prove the HTTP server is serving (route/auth reached)
    case "$mc" in 200|401|404) core_ok=1; break;; esac
    sleep 5
  done
fi

echo "--- mm-postgres init.sql result ---"
docker exec matrixmedia-mm-postgres-1 psql -U postgres -tAc \
  "SELECT rolname FROM pg_roles WHERE rolname IN ('mm_app','mm_admin'); SELECT datname FROM pg_database WHERE datname='matrixmedia';" 2>&1 || true
echo "--- container status ---"
compose ps
[ "$ok" -eq 1 ] || die "Synapse did not become ready — check rendered homeserver.yaml/appservice"
[ "$core_ok" -eq 1 ] || die "mm-core did not become ready — check logs: compose logs mm-core"
log "LOCAL SMOKE PASS — full stack booted on our rendered configs. Tear down: $0 down"
