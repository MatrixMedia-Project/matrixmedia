# shellcheck shell=bash
: "${MM_ROOT:=/opt/mm}"

# wait_healthy PROJECT [TIMEOUT_SEC] -- 0 once no container is "health: starting"
# and none is unhealthy/Exited/Restarting; 1 on timeout. Containers without a
# healthcheck count as fine once "Up". `|| true` keeps grep-no-match from
# tripping set -e/pipefail.
wait_healthy() {
  local proj="$1" timeout="${2:-300}" waited=0 statuses starting bad
  while :; do
    statuses="$(docker ps -a --filter "label=com.docker.compose.project=$proj" --format '{{.Status}}')"
    starting="$(printf '%s\n' "$statuses" | grep -c 'health: starting' || true)"
    bad="$(printf '%s\n' "$statuses" | grep -cE 'unhealthy|Exited|Restarting' || true)"
    if [ "$starting" -eq 0 ] && [ "$bad" -eq 0 ]; then return 0; fi
    waited=$((waited + 5))
    if [ "$waited" -ge "$timeout" ]; then
      docker ps --filter "label=com.docker.compose.project=$proj"
      return 1
    fi
    sleep 5
  done
}

# stack_up [PROJECT] -- pull, up -d, wait healthy. Uses both env files.
stack_up() {
  local proj="${1:-matrixmedia}"
  docker compose --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -p "$proj" pull
  docker compose --env-file "$MM_ROOT/.env" --env-file "$MM_ROOT/.env.secrets" \
    -f "$MM_ROOT/docker-compose.yml" -p "$proj" up -d --remove-orphans
  wait_healthy "$proj" 300 || die "stack did not become healthy"
}
