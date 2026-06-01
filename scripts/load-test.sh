#!/usr/bin/env bash
# MatrixMedia Load Test -- 50 Concurrent Viewers
# Prerequisites: mm-core running, Synapse running, a stream active
# Usage: bash scripts/load-test.sh [NUM_VIEWERS] [MM_URL]

set -euo pipefail

NUM_VIEWERS="${1:-50}"
MM_URL="${2:-http://localhost:6167}"
HS_URL="${3:-http://localhost:8008}"
ADMIN_TOKEN="${MM_ADMIN_TOKEN:-dev-admin-token-change-this-in-prod}"

echo "=== MatrixMedia Load Test ==="
echo "Target: $NUM_VIEWERS concurrent viewers"
echo ""

# Step 1: Create test users and get tokens
echo "Step 1: Creating $NUM_VIEWERS test users..."
declare -a MM_TOKENS

for i in $(seq 1 "$NUM_VIEWERS"); do
    USER="mmload${i}"
    # Register (ignore if exists)
    curl -sf -X POST "$HS_URL/_matrix/client/v3/register" \
      -H "Content-Type: application/json" \
      -d "{\"username\":\"$USER\",\"password\":\"loadtest123\",\"auth\":{\"type\":\"m.login.dummy\"}}" 2>/dev/null || true

    # Login
    LOGIN=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/login" \
      -H "Content-Type: application/json" \
      -d "{\"type\":\"m.login.password\",\"user\":\"$USER\",\"password\":\"loadtest123\"}")

    ACCESS_TOKEN=$(echo "$LOGIN" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)
    USER_ID=$(echo "$LOGIN" | grep -o '"user_id":"[^"]*"' | cut -d'"' -f4)

    # Get OpenID token
    OPENID=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/user/$USER_ID/openid/request_token" \
      -H "Authorization: Bearer $ACCESS_TOKEN" \
      -H "Content-Type: application/json" -d '{}')
    OPENID_TOKEN=$(echo "$OPENID" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)

    # Exchange for MM token
    AUTH=$(curl -sf -X POST "$MM_URL/_mm/client/v1/auth/token" \
      -H "Content-Type: application/json" \
      -d "{\"openid_token\":{\"access_token\":\"$OPENID_TOKEN\",\"token_type\":\"Bearer\",\"matrix_server_name\":\"localhost\",\"expires_in\":3600}}")
    MM_TOKEN=$(echo "$AUTH" | grep -o '"mm_token":"[^"]*"' | cut -d'"' -f4)

    MM_TOKENS[$i]="$MM_TOKEN"
    printf "\r  Users created: $i/$NUM_VIEWERS"
done
echo ""

# Step 2: Create a host user and start a stream
echo "Step 2: Starting stream..."
# Use first user as host
HOST_TOKEN="${MM_TOKENS[1]}"

# Create a room first
HOST_LOGIN=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/login" \
  -H "Content-Type: application/json" \
  -d '{"type":"m.login.password","user":"mmload1","password":"loadtest123"}')
HOST_ACCESS=$(echo "$HOST_LOGIN" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)

ROOM=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/createRoom" \
  -H "Authorization: Bearer $HOST_ACCESS" \
  -H "Content-Type: application/json" \
  -d '{"name":"Load Test Room","preset":"public_chat"}')
ROOM_ID=$(echo "$ROOM" | grep -o '"room_id":"[^"]*"' | cut -d'"' -f4)

STREAM=$(curl -sf -X POST "$MM_URL/_mm/client/v1/streams" \
  -H "Authorization: Bearer $HOST_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"room_id\":\"$ROOM_ID\",\"media_type\":\"audio\",\"title\":\"Load Test\"}")
STREAM_ID=$(echo "$STREAM" | grep -o '"stream_id":"[^"]*"' | cut -d'"' -f4)
echo "  Stream: $STREAM_ID"

# Step 3: Join all viewers concurrently
echo "Step 3: Joining $((NUM_VIEWERS - 1)) viewers concurrently..."
START_TIME=$(date +%s%N)

PIDS=()
JOIN_RESULTS="/tmp/mm-load-results"
rm -rf "$JOIN_RESULTS" && mkdir -p "$JOIN_RESULTS"

for i in $(seq 2 "$NUM_VIEWERS"); do
    (
        RESULT=$(curl -sf -w "\n%{http_code}\n%{time_total}" -X POST \
          "$MM_URL/_mm/client/v1/streams/$STREAM_ID/join" \
          -H "Authorization: Bearer ${MM_TOKENS[$i]}" \
          -H "Content-Type: application/json" \
          -H "Idempotency-Key: load-$i-$(date +%s)" \
          -d '{}' 2>&1)

        HTTP_CODE=$(echo "$RESULT" | tail -2 | head -1)
        LATENCY=$(echo "$RESULT" | tail -1)

        echo "$i $HTTP_CODE $LATENCY" > "$JOIN_RESULTS/$i.txt"
    ) &
    PIDS+=($!)
done

# Wait for all joins
for pid in "${PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
done

END_TIME=$(date +%s%N)
TOTAL_MS=$(( (END_TIME - START_TIME) / 1000000 ))

# Step 4: Analyze results
echo "Step 4: Results"
echo ""

SUCCESS=0
FAIL=0
LATENCIES=()

for f in "$JOIN_RESULTS"/*.txt; do
    read -r VIEWER CODE LATENCY < "$f"
    if [ "$CODE" = "200" ]; then
        SUCCESS=$((SUCCESS + 1))
        LATENCIES+=("$LATENCY")
    else
        FAIL=$((FAIL + 1))
    fi
done

echo "  Total viewers: $((NUM_VIEWERS - 1))"
echo "  Successful joins: $SUCCESS"
echo "  Failed joins: $FAIL"
echo "  Total wall time: ${TOTAL_MS}ms"

if [ ${#LATENCIES[@]} -gt 0 ]; then
    # Sort latencies and get p50, p95, p99
    SORTED=($(printf '%s\n' "${LATENCIES[@]}" | sort -n))
    P50_IDX=$(( ${#SORTED[@]} * 50 / 100 ))
    P95_IDX=$(( ${#SORTED[@]} * 95 / 100 ))
    P99_IDX=$(( ${#SORTED[@]} * 99 / 100 ))
    echo "  Join latency p50: ${SORTED[$P50_IDX]}s"
    echo "  Join latency p95: ${SORTED[$P95_IDX]}s"
    echo "  Join latency p99: ${SORTED[$P99_IDX]}s"
fi

# Step 5: Check metrics
echo ""
echo "Step 5: Metrics"
METRICS=$(curl -sf "http://localhost:9090/metrics" 2>&1 || echo "")
ACTIVE=$(echo "$METRICS" | grep "mm_streams_active " | awk '{print $2}')
PARTICIPANTS=$(echo "$METRICS" | grep "mm_participant_count " | awk '{print $2}')
echo "  Active streams: ${ACTIVE:-N/A}"
echo "  Active participants: ${PARTICIPANTS:-N/A}"

# Step 6: Cleanup -- end stream
echo ""
echo "Step 6: Cleanup"
curl -sf -X POST "$MM_URL/_mm/client/v1/streams/$STREAM_ID/end" \
  -H "Authorization: Bearer $HOST_TOKEN" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: load-end-$(date +%s)" \
  -d '{}' > /dev/null 2>&1

echo "  Stream ended."
rm -rf "$JOIN_RESULTS"

# Verdict
echo ""
echo "================================="
if [ $FAIL -eq 0 ]; then
    echo "PASS -- All $SUCCESS viewers joined successfully"
else
    echo "WARN -- $FAIL/$((NUM_VIEWERS-1)) viewers failed to join"
fi
