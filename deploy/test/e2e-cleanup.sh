#!/usr/bin/env bash
# Delete the throwaway e2e VM. Safe to run regardless of test outcome (CI calls
# this with if: always()). Matches by name "mm-e2e-$RUN". --dry-run prints only.
set -euo pipefail
API="https://api.hetzner.cloud/v1"
DRY=0; RUN="${GITHUB_RUN_ID:-local}"
while [ $# -gt 0 ]; do case "$1" in
  --dry-run) DRY=1;; --run) RUN="$2"; shift;;
  *) echo "unknown arg: $1" >&2; exit 2;; esac; shift; done
NAME="mm-e2e-$RUN"

if [ "$DRY" -eq 1 ]; then
  echo "[dry-run] GET $API/servers?name=$NAME -> id"
  echo "[dry-run] DELETE $API/servers/<id>"
  echo "e2e-cleanup dry-run OK"
  exit 0
fi

TOKEN="${HETZNER_API_TOKEN:?HETZNER_API_TOKEN required}"
ID="$(curl -fsSL -H "Authorization: Bearer $TOKEN" "$API/servers?name=$NAME" | jq -r '.servers[0].id // empty')"
if [ -z "$ID" ]; then echo "no VM named $NAME — nothing to delete"; exit 0; fi
curl -fsSL -X DELETE -H "Authorization: Bearer $TOKEN" "$API/servers/$ID" >/dev/null
echo "deleted $NAME (id $ID)"
