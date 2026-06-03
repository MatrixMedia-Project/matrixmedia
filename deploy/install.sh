#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${MM_ROOT:=/opt/mm}"
for f in common preflight dns secrets render up smoke bootstrap backup; do
  # shellcheck disable=SC1090
  source "$HERE/lib/$f.sh"
done

DRY=0; DOMAIN=""; EMAIL=""; DNS_TOKEN=""; SUBDOMAIN=""; DEMO=false; NONINT=0
while [ $# -gt 0 ]; do case "$1" in
  --dry-run) DRY=1;; --domain) DOMAIN="$2"; shift;; --email) EMAIL="$2"; shift;;
  --dns-token) DNS_TOKEN="$2"; shift;; --vendor-subdomain) SUBDOMAIN="$2"; shift;;
  --demo) DEMO=true;; --non-interactive) NONINT=1;;
  -h|--help) echo "usage: install.sh --domain D --email E [--dns-token T] [--vendor-subdomain S] [--demo] [--non-interactive] [--dry-run]"; exit 0;;
  *) die "unknown arg $1";; esac; shift; done

if [ "$DRY" -eq 1 ]; then log "dry-run OK (libs sourced, args parsed: domain=$DOMAIN demo=$DEMO)"; exit 0; fi

[ "$(id -u)" -eq 0 ] || die "run as root"
mkdir -p "$MM_ROOT"; chmod 700 "$MM_ROOT"     # host-side guard: rendered configs hold secrets
cp -r "$HERE/templates" "$MM_ROOT/"; cp "$HERE/docker-compose.tmpl.yml" "$MM_ROOT/"

preflight
if [ -n "$SUBDOMAIN" ]; then DOMAIN="$SUBDOMAIN.matrixmedia.app"; fi
[ -n "$DOMAIN" ] || die "domain required (--domain or --vendor-subdomain)"
[ -n "$EMAIL" ]  || die "email required (--email)"
cat > "$MM_ROOT/.env" <<EOF
MM_ROOT=$MM_ROOT
MM_DOMAIN=$DOMAIN
MM_PUBLIC_IP=$PUBLIC_IP
ACME_EMAIL=$EMAIL
MM_REGISTRY=${MM_REGISTRY:-ghcr.io/matrixmedia}
MM_VERSION=${MM_VERSION:-0.8.1}
MM_SWITCH_VERSION=${MM_SWITCH_VERSION:-0.5.12}
MM_DEMO_MODE=$DEMO
MM_ALLOW_MOCK=$DEMO
MM_STRIPE_API_BASE=$([ "$DEMO" = true ] && echo http://mm-fakestripe:8787/ || echo https://api.stripe.com/)
MM_STRIPE_SECRET_KEY=${MM_STRIPE_SECRET_KEY:-}
MM_STRIPE_WEBHOOK_SECRET=${MM_STRIPE_WEBHOOK_SECRET:-}
EOF
# vendor subdomains are pre-pointed at us; BYO-domain must resolve + picks TLS mode
[ -z "$SUBDOMAIN" ] && dns_gate "$DOMAIN" "$PUBLIC_IP" "$DNS_TOKEN"
echo "MM_TLS_MODE=${MM_TLS_MODE:-http01}" >> "$MM_ROOT/.env"

generate_secrets
write_secret_files
render_templates
stack_up matrixmedia

# bootstrap: provision first admin via registration_shared_secret, capture token,
# re-render mm-core env with the real token, restart just mm-core.
ADMIN_USER="admin"; ADMIN_PASS="$(openssl rand -hex 12)"
docker exec matrixmedia-synapse-1 register_new_matrix_user -c /data/homeserver.yaml \
   -u "$ADMIN_USER" -p "$ADMIN_PASS" -a || warn "admin may already exist"
capture_admin_token "$DOMAIN" "$ADMIN_USER" "$ADMIN_PASS"
render_templates
docker compose --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
   -f "$MM_ROOT/docker-compose.yml" -p matrixmedia up -d mm-core

# shellcheck disable=SC1091
source "$MM_ROOT/.env.secrets"
self_smoke "$DOMAIN" "${MM_ADMIN_TOKEN:-}"
ln -sf "$HERE/mmctl" /usr/local/bin/mmctl
printf '%s\n' "$ADMIN_USER:$ADMIN_PASS" > "$MM_ROOT/admin.credentials"; chmod 600 "$MM_ROOT/admin.credentials"
log "DONE. https://matrix.$DOMAIN  https://call.$DOMAIN  | admin creds: $MM_ROOT/admin.credentials | run: mmctl status"
