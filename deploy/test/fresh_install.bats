load helper

# Regressions found by the first install on a clean server (Hetzner cx23, Ubuntu 24.04,
# the public one-liner, 2026-11-07). Each one stopped every fresh install, and no other
# test noticed: none of them starts the stack on an empty host.

# `[[ ]] || false` throughout: on bash < 4.1 (macOS /bin/bash) a failing [[ ]] that is
# not a test's last command does not fail the test.

# service_block NAME -- the lines of one service in the compose template.
service_block() {
  awk -v s="  $1:" '$0 == s { on = 1; print; next }
    on && (/^  [a-z][a-z0-9-]*:$/ || /^[a-z]/) { exit }
    on { print }' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

# versions.env pins postgres:18. From 18 on, the image keeps its cluster in
# /var/lib/postgresql/18/docker and REFUSES to start when a volume sits at the old
# /var/lib/postgresql/data (docker-library/postgres#1259). PGDATA must name the
# directory the data volume is mounted on.
@test "every postgres service points PGDATA at its data volume's mount" {
  run awk '
    /^  [a-z][a-z0-9-]*:$/ { if (pg) print svc, mount, pgdata; svc=$1; pg=0; mount=""; pgdata="" }
    /image: \$\{POSTGRES_IMAGE/ { pg=1 }
    /-data:\/var\/lib\/postgresql/ { sub(/.*-data:/, ""); mount=$0 }
    /PGDATA:/ { pgdata=$2 }
    END { if (pg) print svc, mount, pgdata }
  ' "$DEPLOY_ROOT/docker-compose.tmpl.yml"
  [ "$status" -eq 0 ]
  [ "${#lines[@]}" -ge 2 ]   # postgres (Synapse) + mm-postgres
  for l in "${lines[@]}"; do
    read -r svc mount pgdata <<<"$l"
    [ -n "$mount" ] && [ "$pgdata" = "$mount" ] || { echo "$svc data volume at ${mount:-?}, PGDATA=${pgdata:-unset}"; return 1; }
  done
}

# The postgres entrypoint runs /docker-entrypoint-initdb.d/* AFTER dropping to the
# postgres user, and the rendered init.sql is root-only (0600: it embeds both db
# passwords). Mounted there, it was unreadable: mm_admin was never created, and the
# cluster still counted as initialised, so no re-run ever retried it.
@test "mm-postgres mounts nothing straight into /docker-entrypoint-initdb.d" {
  run service_block mm-postgres
  [ -n "$output" ]
  [[ "$output" != *":/docker-entrypoint-initdb.d/"* ]] || false
}
@test "mm-postgres copies its init script in for the postgres user while still root" {
  run service_block mm-postgres
  [[ "$output" == *"/config/mm-postgres-init.sql:/run/mm-init/init.sql:ro"* ]] || false
  [[ "$output" == *"install -o postgres -m 0400 /run/mm-init/init.sql /docker-entrypoint-initdb.d/init.sql"*"exec docker-entrypoint.sh"* ]] || false
}

# latest-ci is upstream's rolling build of main. It started requiring
# LIVEKIT_FULL_ACCESS_HOMESERVERS, and every install pulled a container that exited.
@test "lk-jwt-service runs a released version, and compose falls back to the same pin" {
  pin="$(sed -n 's/^LK_JWT_IMAGE=//p' "$DEPLOY_ROOT/versions.env")"
  [[ "$pin" =~ ^ghcr\.io/element-hq/lk-jwt-service:[0-9]+\.[0-9]+\.[0-9]+$ ]] || false
  run service_block lk-jwt-service
  [[ "$output" == *"\${LK_JWT_IMAGE:-$pin}"* ]] || false
}
@test "lk-jwt-service names this server as its only full-access homeserver" {
  run service_block lk-jwt-service
  [[ "$output" == *'LIVEKIT_FULL_ACCESS_HOMESERVERS: ${MM_DOMAIN}'* ]] || false
}
