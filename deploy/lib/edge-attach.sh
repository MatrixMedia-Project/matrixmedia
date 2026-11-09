# shellcheck shell=bash
# Re-attach first-party backends to the external edge network.
#
# OpenPTT Traefik reaches first-party backends by container name over the
# external `shared_network`, which compose does not manage — every recreate
# (workflow deploy AND manual `compose up -d <svc>`) drops the attachment and
# every Traefik route to that backend 502s until something re-attaches it.
# Sourced for log/warn (deploy/lib/common.sh); has no side effects on source.

: "${EDGE_NETWORK:=shared_network}"

# First-party backends Traefik routes to by container name.
EDGE_SERVICES=(mm-core mm-switch mm-fleet-runner mm-media-worker mm-web mm-fakestripe mm-caption-worker)

# edge_attach [SERVICE...] -- 0 once every backend container is on EDGE_NETWORK.
# Defaults to EDGE_SERVICES. Idempotent: already-attached backends are skipped.
edge_attach() {
  local services=("$@") svc container fail=0
  [ "${#services[@]}" -eq 0 ] && services=("${EDGE_SERVICES[@]}")
  for svc in "${services[@]}"; do
    container="matrixmedia-${svc}-1"
    if docker inspect "$container" --format '{{json .NetworkSettings.Networks}}' 2>/dev/null | grep -q "$EDGE_NETWORK"; then
      log "$container: already on $EDGE_NETWORK"
    elif docker network connect "$EDGE_NETWORK" "$container" 2>/dev/null; then
      log "$container: attached to $EDGE_NETWORK"
    else
      warn "$container: attach to $EDGE_NETWORK failed (container missing?)"
      fail=1
    fi
  done
  return "$fail"
}
