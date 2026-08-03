#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${MM_ROOT:=/opt/mm}"
for f in common preflight dns secrets render up smoke bootstrap backup; do
  # shellcheck disable=SC1090
  source "$HERE/lib/$f.sh"
done

DRY=0; DOMAIN=""; EMAIL=""; DNS_TOKEN=""; SUBDOMAIN=""; DEMO=false; NONINT=0; NODOMAIN=0
ADMIN_USER=""; ADMIN_PASS=""
while [ $# -gt 0 ]; do case "$1" in
  --dry-run) DRY=1;; --domain) DOMAIN="$2"; shift;; --email) EMAIL="$2"; shift;;
  --dns-token) DNS_TOKEN="$2"; shift;; --vendor-subdomain) SUBDOMAIN="$2"; shift;;
  --admin-user) ADMIN_USER="$2"; shift;; --admin-pass) ADMIN_PASS="$2"; shift;;
  --demo) DEMO=true;; --non-interactive) NONINT=1;; --no-domain) NODOMAIN=1;;
  -h|--help) echo "usage: install.sh --domain D --email E [--admin-user U --admin-pass P] [--dns-token T] [--vendor-subdomain S] [--demo] [--non-interactive] [--dry-run]"; exit 0;;
  *) die "unknown arg $1";; esac; shift; done

if [ "$DRY" -eq 1 ]; then log "dry-run OK (libs sourced, args parsed: domain=$DOMAIN demo=$DEMO)"; exit 0; fi

[ "$(id -u)" -eq 0 ] || die "run as root"
mkdir -p "$MM_ROOT"; chmod 700 "$MM_ROOT"     # host-side guard: rendered configs hold secrets
cp -r "$HERE/templates" "$MM_ROOT/"; cp "$HERE/docker-compose.tmpl.yml" "$MM_ROOT/"
cp "$HERE/versions.env" "$MM_ROOT/"   # the pinned image set; passed to compose ahead of .env

preflight
MM_TEMP_MODE=false
if [ "$NODOMAIN" -eq 1 ]; then
  [ -z "$DOMAIN$SUBDOMAIN" ] || die "--no-domain conflicts with --domain/--vendor-subdomain"
  DOMAIN="$(sslip_domain "$PUBLIC_IP")"; MM_TEMP_MODE=true
  [ -n "$EMAIL" ] || EMAIL="temp@$DOMAIN"
  warn "TEMP MODE: $DOMAIN is a THROWAWAY identity (self-signed TLS, no federation). Claiming a real domain later means a fresh install."
fi
if [ -n "$SUBDOMAIN" ]; then DOMAIN="$SUBDOMAIN.matrixmedia.app"; fi
[ -n "$DOMAIN" ] || die "domain required (--domain or --vendor-subdomain)"
[ -n "$EMAIL" ]  || die "email required (--email)"
cat > "$MM_ROOT/.env" <<EOF
MM_ROOT=$MM_ROOT
MM_DOMAIN=$DOMAIN
MM_PUBLIC_IP=$PUBLIC_IP
ACME_EMAIL=$EMAIL
MM_DEMO_MODE=$DEMO
MM_TEMP_MODE=$MM_TEMP_MODE
MM_ALLOW_MOCK=$DEMO
MM_STRIPE_API_BASE=$([ "$DEMO" = true ] && echo http://mm-fakestripe:8787/ || echo https://api.stripe.com/)
MM_STRIPE_SECRET_KEY=${MM_STRIPE_SECRET_KEY:-}
MM_STRIPE_WEBHOOK_SECRET=${MM_STRIPE_WEBHOOK_SECRET:-}
MM_RETENTION_ENABLED=${MM_RETENTION_ENABLED:-false}
MM_RETENTION_MIN_LIFETIME=${MM_RETENTION_MIN_LIFETIME:-1d}
MM_RETENTION_MAX_LIFETIME=${MM_RETENTION_MAX_LIFETIME:-90d}
EOF
# Image pins live in versions.env, which compose reads BEFORE .env. Do not restate them
# here: .env wins, so writing a stale MM_VERSION into it silently overrides the pinned set
# (which is exactly what happened — .env carried MM_REGISTRY=ghcr.io/matrixmedia, an org
# that does not exist, and MM_VERSION=0.8.1). Only an operator's EXPLICIT override is
# persisted.
for v in MM_REGISTRY MM_VERSION MM_SWITCH_VERSION; do
  [ -n "${!v:-}" ] && echo "$v=${!v}" >> "$MM_ROOT/.env"
done

# vendor subdomains are pre-pointed at us; BYO-domain must resolve + picks TLS mode
[ -z "$SUBDOMAIN" ] && dns_gate "$DOMAIN" "$PUBLIC_IP" "$DNS_TOKEN"
echo "MM_TLS_MODE=${MM_TLS_MODE:-http01}" >> "$MM_ROOT/.env"

generate_secrets
write_secret_files
render_templates
stack_up matrixmedia

# bootstrap: provision the OWNER's admin account (so the server owner logs into
# their deployment with the username/password they choose — typically the same
# as their main-service login), capture a token, re-render mm-core env, restart.
# If creds weren't passed: prompt interactively, else fall back to a random admin.
GENERATED_PASS=0
if [ -z "$ADMIN_USER" ] && [ "$NONINT" -eq 0 ]; then
  printf 'Owner username for this server (Matrix localpart, e.g. "alice"): ' >&2; read -r ADMIN_USER
fi
if [ -z "$ADMIN_PASS" ] && [ "$NONINT" -eq 0 ] && [ -n "$ADMIN_USER" ]; then
  printf 'Owner password (input hidden): ' >&2; read -rs ADMIN_PASS; printf '\n' >&2
fi
[ -n "$ADMIN_USER" ] || ADMIN_USER="admin"
if [ -z "$ADMIN_PASS" ]; then
  # Converge on re-runs: reuse the password we generated last time, otherwise
  # capture_admin_token dies on "admin exists with a different password".
  ADMIN_PASS="$(grep -s '^MM_OWNER_BOOTSTRAP_PASS=' "$MM_ROOT/.env.secrets" | cut -d= -f2- || true)"
  if [ -n "$ADMIN_PASS" ]; then GENERATED_PASS=1; else
    ADMIN_PASS="$(openssl rand -hex 12)"; GENERATED_PASS=1
    _upsert_secret MM_OWNER_BOOTSTRAP_PASS "$ADMIN_PASS"
  fi
fi
# Validate the localpart so register_new_matrix_user doesn't fail cryptically.
printf '%s' "$ADMIN_USER" | grep -qE '^[a-z0-9._=/-]+$' || die "admin user must be a valid Matrix localpart (lowercase a-z 0-9 . _ = / -)"

docker exec matrixmedia-synapse-1 register_new_matrix_user -c /data/homeserver.yaml \
   -u "$ADMIN_USER" -p "$ADMIN_PASS" -a || warn "owner account may already exist"
capture_admin_token "$DOMAIN" "$ADMIN_USER" "$ADMIN_PASS"
render_templates
# Via compose_env_files, NOT a hand-rolled --env-file pair: this is the installer's LAST
# act, and re-creating mm-core without versions.env would recreate it from the compose
# template's inline default tag — un-pinning, at the finish line, the one service we most
# care about pinning.
compose_env_files
docker compose "${MM_ENV_FILES[@]}" \
   -f "$MM_ROOT/docker-compose.yml" -p matrixmedia up -d mm-core

# shellcheck disable=SC1091
source "$MM_ROOT/.env.secrets"
self_smoke "$DOMAIN" "${MM_ADMIN_TOKEN:-}"
ln -sf "$HERE/mmctl" /usr/local/bin/mmctl
# Record the owner login. Only persist the password if WE generated it; when the
# owner supplied their own we just note the username (their password is theirs).
if [ "$GENERATED_PASS" -eq 1 ]; then
  printf 'owner_user=%s\nowner_pass=%s\nlogin=https://matrix.%s\n' "$ADMIN_USER" "$ADMIN_PASS" "$DOMAIN" > "$MM_ROOT/admin.credentials"
else
  printf 'owner_user=%s\nowner_pass=(the password you chose)\nlogin=https://matrix.%s\n' "$ADMIN_USER" "$DOMAIN" > "$MM_ROOT/admin.credentials"
fi
chmod 600 "$MM_ROOT/admin.credentials"
[ "$MM_TEMP_MODE" = "true" ] && warn "TEMP install: expect a browser certificate warning; this server cannot federate and its identity is disposable."
log "DONE. Sign in at https://matrix.$DOMAIN as @$ADMIN_USER:$DOMAIN (server owner/admin). call: https://call.$DOMAIN | creds: $MM_ROOT/admin.credentials | run: mmctl status"
