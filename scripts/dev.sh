#!/usr/bin/env bash
# Start MatrixMedia development environment
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=== MatrixMedia Dev Environment ==="
echo "Project root: $PROJECT_ROOT"
echo ""

# --- Start infrastructure ---
echo "Starting infrastructure..."
cd "$PROJECT_ROOT/infra/docker"
cp -n .env.example .env 2>/dev/null || true
docker compose up -d
cd "$PROJECT_ROOT"

echo ""
echo "Waiting for services..."

# Wait for Synapse
printf "  Synapse: "
for i in $(seq 1 30); do
    if curl -sf http://localhost:8008/_matrix/client/versions > /dev/null 2>&1; then
        echo "ready"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "TIMEOUT (waited 30s)"
        echo "  Check: docker compose -f infra/docker/docker-compose.yml logs synapse"
        exit 1
    fi
    sleep 1
done

# Wait for LiveKit
printf "  LiveKit: "
for i in $(seq 1 15); do
    if curl -sf http://localhost:7880 > /dev/null 2>&1; then
        echo "ready"
        break
    fi
    if [ "$i" -eq 15 ]; then
        echo "TIMEOUT (waited 15s)"
        echo "  Check: docker compose -f infra/docker/docker-compose.yml logs livekit"
        exit 1
    fi
    sleep 1
done

# Wait for MinIO
printf "  MinIO:   "
for i in $(seq 1 15); do
    if curl -sf http://localhost:9001 > /dev/null 2>&1; then
        echo "ready"
        break
    fi
    if [ "$i" -eq 15 ]; then
        echo "TIMEOUT (waited 15s)"
        echo "  Check: docker compose -f infra/docker/docker-compose.yml logs minio"
        exit 1
    fi
    sleep 1
done

# Check coturn (just verify the container is running)
printf "  coturn:  "
if docker compose -f "$PROJECT_ROOT/infra/docker/docker-compose.yml" ps coturn 2>/dev/null | grep -q "running"; then
    echo "ready"
else
    echo "not running (TURN/STUN unavailable -- WebRTC may fail outside LAN)"
fi

echo ""

# --- Run migrations ---
echo "Running database migrations..."
cargo run -p mm-server -- migrate
echo ""

# --- Start mm-core ---
echo "Starting mm-core..."
echo "  Client API: http://localhost:6167"
echo "  Admin API:  http://localhost:6168"
echo "  Metrics:    http://localhost:9090"
echo ""
echo "  Press Ctrl+C to stop."
echo ""
cargo run -p mm-server -- serve
