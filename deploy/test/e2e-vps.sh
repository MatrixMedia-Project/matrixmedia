#!/usr/bin/env bash
# End-to-end installer smoke on a throwaway Hetzner Cloud VM.
# Real run needs: HETZNER_API_TOKEN, HETZNER_SSH_KEY (key name registered in HC),
# SSH_PRIVATE_KEY available to the agent. --dry-run prints the API/ssh plan only.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$HERE/.." && pwd)"
API="https://api.hetzner.cloud/v1"

DRY=0
RUN="${GITHUB_RUN_ID:-local}"
TYPE="cx22"; IMAGE="ubuntu-24.04"; LOCATION="nbg1"
while [ $# -gt 0 ]; do case "$1" in
  --dry-run) DRY=1;; --run) RUN="$2"; shift;; --type) TYPE="$2"; shift;;
  *) echo "unknown arg: $1" >&2; exit 2;; esac; shift; done
NAME="mm-e2e-$RUN"
SUBDOMAIN="ci-$RUN"

hc() { # METHOD PATH [JSON-BODY]
  local method="$1" path="$2" body="${3:-}"
  if [ "$DRY" -eq 1 ]; then
    echo "[dry-run] curl -X $method $API$path ${body:+-d $body}"
    return 0
  fi
  curl -fsSL -X "$method" \
    -H "Authorization: Bearer ${HETZNER_API_TOKEN:?HETZNER_API_TOKEN required}" \
    -H "Content-Type: application/json" \
    ${body:+-d "$body"} "$API$path"
}

remote() { # run a command on the VM (printed only in dry-run)
  if [ "$DRY" -eq 1 ]; then echo "[dry-run] ssh root@<vm-ip> $*"; return 0; fi
  ssh -o StrictHostKeyChecking=no -o ConnectTimeout=15 "root@$IP" "$@"
}

echo "== provision $NAME ($TYPE/$IMAGE/$LOCATION) =="
CREATE_BODY="{\"name\":\"$NAME\",\"server_type\":\"$TYPE\",\"image\":\"$IMAGE\",\"location\":\"$LOCATION\",\"ssh_keys\":[\"${HETZNER_SSH_KEY:-mm-ci}\"]}"
RESP="$(hc POST /servers "$CREATE_BODY")"

if [ "$DRY" -eq 1 ]; then
  echo "[dry-run] would: wait for SSH, scp -r $DEPLOY_DIR root@<vm-ip>:/root/deploy"
  echo "[dry-run] would: ssh root@<vm-ip> 'bash /root/deploy/install.sh --non-interactive --vendor-subdomain $SUBDOMAIN --email ci@matrixmedia.app'"
  echo "[dry-run] would: ssh root@<vm-ip> 'mmctl doctor'  (exit 0 == pass)"
  echo "[dry-run] would: ssh root@<vm-ip> 'mmctl backup && ls /opt/mm/backups/*.tar.gz'"
  echo "e2e dry-run OK"
  exit 0
fi

IP="$(echo "$RESP" | jq -r '.server.public_net.ipv4.ip')"
if [ -z "$IP" ] || [ "$IP" = "null" ]; then echo "no IP from create response" >&2; exit 1; fi
echo "vm ip: $IP — waiting for SSH"
for _ in $(seq 1 40); do ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 "root@$IP" true 2>/dev/null && break; sleep 10; done

scp -o StrictHostKeyChecking=no -r "$DEPLOY_DIR" "root@$IP:/root/deploy"
remote "bash /root/deploy/install.sh --non-interactive --vendor-subdomain $SUBDOMAIN --email ci@matrixmedia.app"
remote "mmctl doctor"
remote "mmctl backup && ls /opt/mm/backups/*.tar.gz"
echo "e2e PASS on $IP"
