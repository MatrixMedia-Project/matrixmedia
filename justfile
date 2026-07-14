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
    echo "── toolchain ──"

    want_rust="$(grep -E '^channel' rust-toolchain.toml | cut -d'"' -f2)"
    have_rust="$(rustc --version 2>/dev/null | awk '{print $2}')"
    if [ "$have_rust" = "$want_rust" ]; then
      echo "  rust     $have_rust"
    else
      # rustup honours rust-toolchain.toml automatically, so this is usually just "not
      # installed yet" rather than a real mismatch.
      echo "  rust     $have_rust  (repo pins $want_rust — rustup will fetch it on first build)"
    fi

    want_node="$(cat .nvmrc)"
    have_node="$(node --version 2>/dev/null | sed 's/^v//')"
    if [ "$have_node" = "$want_node" ]; then
      echo "  node     $have_node"
    else
      echo "  node     ${have_node:-MISSING}  (repo pins $want_node — run: nvm use)"; fail=1
    fi

    for tool in docker jq; do
      if command -v "$tool" >/dev/null; then echo "  $tool     ok"
      else echo "  $tool     MISSING"; fail=1; fi
    done
    docker compose version >/dev/null 2>&1 && echo "  compose  ok" || { echo "  compose  MISSING (need Compose v2)"; fail=1; }

    echo "── notes ──"
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

# Same as `dev`, but print the URLs a PHYSICAL DEVICE must use.
dev-lan:
    #!/usr/bin/env bash
    set -uo pipefail
    ip="$(ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}')"
    if [ -z "${ip:-}" ]; then echo "could not detect a LAN IP — pass it by hand"; exit 1; fi
    MM_PUBLIC_HOST="http://$ip:8008" {{DC}} up -d
    echo ""
    echo "  LAN IP: $ip"
    echo "  Point the phone at:  http://$ip:8008"
    echo ""
    echo "  Android: echo 'mm.homeserver=http://$ip:8008' >> Production/android/local.properties"
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
    docker compose -f infra/docker/docker-compose.yml exec -T synapse \
      register_new_matrix_user -c /data/homeserver.yaml -u {{user}} -p {{pass}} -a \
      http://localhost:8008 || echo "(user may already exist)"
    echo "user: @{{user}}:localhost  password: {{pass}}"

# ── tests ────────────────────────────────────────────────────────────────────

# Rust tests WITHOUT a database. Fast — but see the warning it prints.
test:
    #!/usr/bin/env bash
    set -uo pipefail
    cargo test --all
    echo ""
    echo "  ⚠  MM_DATABASE_URL is unset, so every DB-gated test SKIPPED — and a skipped"
    echo "     test still counts as PASSED. This suite going green does NOT mean the SQL,"
    echo "     the migrations, or the feed fan-out were executed at all."
    echo "     For the real thing:  just test-db"

# `cargo test --all` alone silently skips every DB-gated test and still reports green —
# which is exactly how a broken migration and a broken fan-out query shipped. MM_REQUIRE_DB
# makes a missing/unreachable database a hard failure instead of a silent skip.

# Rust tests WITH a real Postgres. THIS is the suite that actually exercises the SQL.
test-db:
    #!/usr/bin/env bash
    set -euo pipefail
    name="mm-test-pg"
    trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" \
      -e POSTGRES_PASSWORD=mm -e POSTGRES_DB=mm_test -p 55432:5432 \
      postgres:16-alpine >/dev/null
    echo "waiting for postgres..."
    for _ in $(seq 1 30); do
      docker exec "$name" pg_isready -U postgres >/dev/null 2>&1 && break
      sleep 1
    done
    MM_DATABASE_URL="postgres://postgres:mm@localhost:55432/mm_test" \
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
