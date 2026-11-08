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

# mm-core refuses to start with monetization on and a Stripe secret empty, and the
# compose default is on. A plain install (no --demo, no Stripe account yet) never
# started mm-core, and the Operator Console where payments are turned on is served by
# that same mm-core. --demo had the same hole: nothing generated keys for mm-fakestripe.
@test "payments_seed turns payments on only when both Stripe secrets are set" {
  source "$DEPLOY_ROOT/lib/common.sh"
  [ "$(payments_seed "" "")" = false ]
  [ "$(payments_seed sk_test_x "")" = false ]
  [ "$(payments_seed "" whsec_y)" = false ]
  [ "$(payments_seed sk_test_x whsec_y)" = true ]
}
@test "install.sh seeds MM_MONETIZATION_ENABLED in .env from the Stripe secrets" {
  grep -qF 'PAYMENTS="${MM_MONETIZATION_ENABLED:-$(payments_seed "${MM_STRIPE_SECRET_KEY:-}" "${MM_STRIPE_WEBHOOK_SECRET:-}")}"' "$DEPLOY_ROOT/install.sh"
  env_block="$(sed -n '/^cat > "\$MM_ROOT\/.env" <<EOF$/,/^EOF$/p' "$DEPLOY_ROOT/install.sh")"
  [[ "$env_block" == *'MM_MONETIZATION_ENABLED=$PAYMENTS'* ]] || false
}
@test "install.sh --demo gives mm-fakestripe throwaway test keys" {
  demo="$(sed -n '/^if \[ "\$DEMO" = true \]; then$/,/^fi$/p' "$DEPLOY_ROOT/install.sh")"
  [[ "$demo" == *'MM_STRIPE_SECRET_KEY:=sk_test_demo_'* ]] || false
  [[ "$demo" == *'MM_STRIPE_WEBHOOK_SECRET:=whsec_demo_'* ]] || false
}
@test "install.sh says how to turn payments on when it left them off" {
  grep -q '"$PAYMENTS" = false \].*Settings → Monetization' "$DEPLOY_ROOT/install.sh"
}

# Synapse, mm-core, mm-web, mm-switch and LiveKit each sit on two networks. Traefik's
# docker provider needs the network's real name, which compose prefixes with the
# project: "mm_network" matched nothing, so Traefik picked one per container at random,
# and routes that landed on `internal` timed out (504 on /_matrix/client/v3/login).
@test "traefik's docker provider names mm_network by its compose-prefixed name" {
  project="$(sed -n 's/^stack_up \([a-z0-9_-]*\)$/\1/p' "$DEPLOY_ROOT/install.sh")"
  [ -n "$project" ]
  net="$(sed -n '/^networks:/,$p' "$DEPLOY_ROOT/docker-compose.tmpl.yml" | sed -n '/^  mm_network:$/,/^  [a-z]/p')"
  [ -n "$net" ]
  [[ "$net" != *"name:"* ]] || false   # no fixed name: compose's <project>_mm_network applies
  grep -qF -- "--providers.docker.network=${project}_mm_network" "$DEPLOY_ROOT/docker-compose.tmpl.yml"
}

# install.sh recreates mm-core (to hand it the captured admin token) and then probes
# it. Probing before it is healthy and routed made the self-smoke fail on a stack
# that was fine a few seconds later.
@test "install.sh waits for the recreated mm-core to be healthy and routed before the smoke" {
  between="$(sed -n '/ up -d mm-core$/,/^self_smoke /p' "$DEPLOY_ROOT/install.sh")"
  [[ "$between" == *"wait_healthy matrixmedia"* ]] || false
  [[ "$between" == *'wait_http "https://matrix.$DOMAIN/mm/v1/announcements/active" 200'* ]] || false
}

# Clients find a homeserver from "@alice:example.com" by fetching
# https://example.com/.well-known/matrix/client. mm-web serves it, but Traefik only
# forwarded /server on the apex, so the lookup (and the installer's smoke probe) got 404.
@test "the apex forwards both Matrix well-known files to mm-web" {
  block="$(awk '/^    [a-z][a-z0-9-]*:$/ { if (b ~ /Host\(`\$\{MM_DOMAIN\}`\) && .*well-known\/matrix/) print b; b="" } { b = b $0 "\n" }
    END { if (b ~ /Host\(`\$\{MM_DOMAIN\}`\) && .*well-known\/matrix/) print b }' "$DEPLOY_ROOT/templates/traefik-dynamic.tmpl.yaml")"
  [ -n "$block" ]
  [[ "$block" == *'PathPrefix(`/.well-known/matrix/`)'* ]] || false
  [[ "$block" == *"service: mm-web"* ]] || false
  grep -q 'location = /.well-known/matrix/client' "$DEPLOY_ROOT/templates/nginx.tmpl.conf"
}

# The failure messages say "run: mmctl logs", but mmctl was linked only after the smoke
# passed, so an operator whose install failed did not have the command it named.
@test "install.sh links mmctl before it starts the stack" {
  ln_at="$(grep -n '^ln -sf "\$HERE/mmctl" /usr/local/bin/mmctl$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  up_at="$(grep -n '^stack_up matrixmedia$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  [ -n "$ln_at" ] && [ -n "$up_at" ]
  [ "$ln_at" -lt "$up_at" ]
}
