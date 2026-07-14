# MatrixMedia task runner.  `just` with no argument lists everything.
#
# Thin wrappers over the existing scripts — nothing here replaces them, and every command
# still works if you run it by hand. The point is that the knowledge lives in the repo
# instead of in someone's head.
#
# Install: brew install just   (or: cargo install just)

set shell := ["bash", "-uc"]

DC := "docker compose -f infra/docker/docker-compose.yml"

# Show the available tasks.
default:
    @just --list --unsorted

# ── environment ──────────────────────────────────────────────────────────────

# Check your toolchain matches what the repo pins. Run this FIRST if anything is odd.
doctor:
    #!/usr/bin/env bash
    set -uo pipefail
    fail=0
    # Single quotes: backticks inside double quotes are command substitution, so
    # "required by `just ci`" would literally RUN `just ci` from inside doctor — and `ci`
    # depends on `doctor`, so that recurses.
    echo '── required by: just ci ──'

    # HARD requirements: exactly the tools `ci` actually invokes. `doctor` used to exit 1 on
    # a node mismatch — which no `ci` step uses — so a Rust/Go contributor on node 22 could
    # not run `just ci` at all; and it never checked go/bats/cargo, so someone WITH node and
    # WITHOUT bats got "doctor: OK", sat through the whole DB and race suites, and only then
    # hit "bats: command not found". Gate on what is used.
    for tool in cargo docker go bats; do
      if command -v "$tool" >/dev/null; then echo "  $tool"$'\t'"ok"
      else echo "  $tool"$'\t'"MISSING"; fail=1; fi
    done
    docker compose version >/dev/null 2>&1 && echo "  compose"$'\t'"ok" || { echo "  compose"$'\t'"MISSING (need Compose v2)"; fail=1; }

    want_rust="$(grep -E '^channel' rust-toolchain.toml | cut -d'"' -f2)"
    have_rust="$(rustc --version 2>/dev/null | awk '{print $2}')"
    # rustup honours rust-toolchain.toml automatically, so a mismatch here is normally just
    # "not fetched yet", not a problem.
    [ "$have_rust" = "$want_rust" ] \
      && echo "  rust"$'\t'"$have_rust" \
      || echo "  rust"$'\t'"${have_rust:-?}  (repo pins $want_rust — rustup fetches it on first build)"

    echo "── advisory (only needed for the web workspace) ──"
    want_node="$(cat .nvmrc)"
    have_node="$(node --version 2>/dev/null | sed 's/^v//')"
    [ "$have_node" = "$want_node" ] \
      && echo "  node"$'\t'"$have_node" \
      || echo "  node"$'\t'"${have_node:-missing}  (repo pins $want_node — 'nvm use'; not needed for just ci)"
    command -v jq >/dev/null && echo "  jq"$'\t'"ok" || echo "  jq"$'\t'"missing (optional)"
    echo "  npm, not pnpm — the repo has web/package-lock.json"

    [ "$fail" -eq 0 ] && echo "doctor: OK" || { echo "doctor: FIX THE ABOVE"; exit 1; }

# ── dev stack ────────────────────────────────────────────────────────────────

# `synapse-init` generates Synapse's config on a fresh volume, so this works on a clone.
# Traefik-dependent services are behind `--profile prod-edge` and are NOT started; without
# that, compose died on "network traefik declared as external, but could not be found".

# Bring up the local infrastructure (Synapse, LiveKit, Postgres, Redis, MinIO, coturn).
dev:
    {{DC}} up -d
    @echo ""
    @echo "  Synapse   http://localhost:8008"
    @echo "  LiveKit   ws://localhost:7880"
    @echo "  MinIO     http://localhost:9001"
    @echo ""
    @echo "  next: just seed   (create a test user)"

# A phone is not this machine: `localhost` on the handset is the handset. Point the app at
# the LAN IP or nothing will connect, and the failure looks like a server bug.

# Same as `dev`, but make Synapse advertise the LAN address a PHYSICAL DEVICE must use.
dev-lan:
    #!/usr/bin/env bash
    set -euo pipefail
    ip="$(ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}' || true)"
    if [ -z "${ip:-}" ]; then echo "could not detect a LAN IP — set public_baseurl by hand"; exit 1; fi
    want="http://$ip:8008"

    MM_PUBLIC_HOST="$want" {{DC}} up -d

    # MM_PUBLIC_HOST is read ONLY by synapse-init, which no-ops when a config already exists.
    # So on any volume that has already been through `just dev`, the env var changes nothing:
    # Synapse keeps advertising public_baseurl: http://localhost:8008/ while this recipe
    # cheerfully prints a LAN address. The phone then hits exactly the mismatch this recipe
    # exists to prevent — and is told the setup is correct. Reconcile it for real.
    cfg=/data/homeserver.yaml
    have="$({{DC}} exec -T synapse sh -c "grep -E '^public_baseurl:' $cfg | head -1" 2>/dev/null || true)"
    if ! printf '%s' "$have" | grep -qF "$want"; then
      echo "  public_baseurl is $have — rewriting to $want/"
      {{DC}} exec -T synapse sh -c \
        "sed -i 's|^public_baseurl:.*|public_baseurl: \"$want/\"|' $cfg"
      {{DC}} restart synapse >/dev/null
      for _ in $(seq 1 40); do
        curl -fsS "http://localhost:8008/_matrix/client/versions" >/dev/null 2>&1 && break
        sleep 3
      done
    fi

    # Assert it, rather than trust it.
    got="$({{DC}} exec -T synapse sh -c "grep -E '^public_baseurl:' $cfg | head -1")"
    printf '%s' "$got" | grep -qF "$want" || { echo "FAILED to set public_baseurl (got: $got)"; exit 1; }

    echo ""
    echo "  LAN IP: $ip   (Synapse now advertises $want/)"
    echo "  Point the phone at:  $want"
    echo ""
    echo "  Android: echo 'mm.homeserver=$want' >> Production/android/local.properties"
    echo "  iOS:     set MM_HOMESERVER in Production/ios/Local.xcconfig"

# Stop the dev stack (keeps volumes).
dev-down:
    {{DC}} down

# Stop the dev stack and DESTROY its data.
dev-reset:
    {{DC}} down -v

# Create a local test user on the dev Synapse.
seed user="alice" pass="alice":
    #!/usr/bin/env bash
    set -uo pipefail
    if ! curl -fsS http://localhost:8008/_matrix/client/versions >/dev/null 2>&1; then
      echo "Synapse is not up. Run: just dev"; exit 1
    fi
    # "Already exists" is the ONLY failure worth forgiving. A blanket `|| echo` would also
    # swallow "registration is disabled", "no shared secret", "config broken" — and since
    # CI's fresh-clone job uses this as its final assertion, that would make the assertion
    # incapable of failing. Match the benign case, re-raise everything else.
    out="$(docker compose -f infra/docker/docker-compose.yml exec -T synapse \
      register_new_matrix_user -c /data/homeserver.yaml -u {{user}} -p {{pass}} -a \
      http://localhost:8008 2>&1)"; rc=$?
    if [ "$rc" -ne 0 ]; then
      if printf '%s' "$out" | grep -qi "already taken\|already exists"; then
        echo "user @{{user}}:localhost already exists — fine"
      else
        echo "$out"
        echo "seed FAILED (rc=$rc) — this is a real error, not 'already exists'"
        exit "$rc"
      fi
    fi
    echo "user: @{{user}}:localhost  password: {{pass}}"

# ── tests ────────────────────────────────────────────────────────────────────

# Rust tests WITHOUT a database. Fast — but see the warning it prints.
test:
    #!/usr/bin/env bash
    # `set -e` is load-bearing. Without it the recipe's exit status is the trailing echo's,
    # so a FAILING cargo run would still exit 0 and `just test` would report green on a red
    # suite — the exact green-while-broken bug this justfile exists to kill.
    set -euo pipefail
    cargo test --all
    if [ -z "${MM_DATABASE_URL:-}" ]; then
      echo ""
      echo "  ⚠  MM_DATABASE_URL is unset, so every DB-gated test SKIPPED — and a skipped"
      echo "     test still counts as PASSED. This suite going green does NOT mean the SQL,"
      echo "     the migrations, or the feed fan-out were executed at all."
      echo "     For the real thing:  just test-db"
    fi

# `cargo test --all` alone silently skips every DB-gated test and still reports green —
# which is exactly how a broken migration and a broken fan-out query shipped. MM_REQUIRE_DB
# makes a missing/unreachable database a hard failure instead of a silent skip.

# Rust tests WITH a real Postgres. THIS is the suite that actually exercises the SQL.
test-db:
    #!/usr/bin/env bash
    set -euo pipefail
    # Per-run container name and an ephemeral host port. A fixed name plus an unconditional
    # `docker rm -f` means a second run (another terminal, or `just ci` alongside a manual
    # run) deletes the FIRST run's database mid-suite, and whichever finishes first removes
    # the other's container on its way out. The victim dies with connection errors that look
    # like a code bug.
    name="mm-test-pg-$$"
    trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
    docker run -d --name "$name" \
      -e POSTGRES_PASSWORD=mm -e POSTGRES_DB=mm_test -P -p 5432 \
      postgres:16-alpine >/dev/null
    port="$(docker port "$name" 5432/tcp | head -1 | sed 's/.*://')"

    echo "waiting for postgres on :$port ..."
    # -h 127.0.0.1 is deliberate. The postgres image runs its bootstrap server on a Unix
    # socket only (listen_addresses=''), and a bare `pg_isready` checks that socket — so it
    # reports READY while TCP is still refusing connections. On a fast machine the loop then
    # breaks early, and because MM_REQUIRE_DB=1 turns an unreachable DB into a panic, the
    # whole suite dies at the first DB test. Probe the transport the tests actually use.
    ready=0
    for _ in $(seq 1 60); do
      if docker exec "$name" pg_isready -h 127.0.0.1 -p 5432 -d mm_test -U postgres >/dev/null 2>&1; then
        ready=1; break
      fi
      sleep 1
    done
    [ "$ready" -eq 1 ] || { echo "postgres never accepted TCP connections"; exit 1; }

    MM_DATABASE_URL="postgres://postgres:mm@localhost:$port/mm_test" \
    MM_REQUIRE_DB=1 \
      cargo test --all

# Go tests for mm-switch, under the race detector.
test-go:
    cd services/mm-switch && go test -race ./...

# Web tests (npm, NOT pnpm).
test-web:
    cd web && npm test

# The deploy/installer suite.
test-deploy:
    bats deploy/test/*.bats

# Everything CI runs, in the order CI runs it. Green here ≈ green there.
ci: doctor test-db test-go test-deploy
    @echo ""
    @echo "ci: OK"
    @echo "note: CI does not enforce fmt/clippy today, and the tree does not pass"
    @echo "      'clippy -D warnings'. Don't add new warnings; don't feel obliged to fix old ones."
