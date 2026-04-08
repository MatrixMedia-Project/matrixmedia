#!/usr/bin/env bash
# MatrixMedia End-to-End Integration Test
# Prerequisites: docker compose running, mm-core running on localhost:6167
# Usage: bash scripts/e2e-test.sh

set -euo pipefail

MM_URL="${MM_URL:-http://localhost:6167}"
ADMIN_URL="${ADMIN_URL:-http://localhost:6168}"
HS_URL="${HS_URL:-http://localhost:8008}"
ADMIN_TOKEN="${MM_ADMIN_TOKEN:-dev-admin-token-change-in-prod}"

PASS=0
FAIL=0
TOTAL=0

# Color output
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

check() {
    local name="$1"
    local result="$2"
    local expected="$3"
    TOTAL=$((TOTAL + 1))
    if echo "$result" | grep -q "$expected"; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC} $name"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC} $name"
        echo "    Expected: $expected"
        echo "    Got: $(echo "$result" | head -3)"
    fi
}

echo "=== MatrixMedia E2E Integration Test ==="
echo ""

# --- Step 0: Check services are up ---
echo "Step 0: Service health checks"

result=$(curl -sf "$ADMIN_URL/_mm/admin/v1/health" -H "Authorization: Bearer $ADMIN_TOKEN" 2>&1 || echo "UNREACHABLE")
check "mm-core admin health" "$result" '"status"'

result=$(curl -sf "$HS_URL/_matrix/client/versions" 2>&1 || echo "UNREACHABLE")
check "Synapse reachable" "$result" '"versions"'

echo ""

# --- Step 1: Create a test user on Synapse ---
echo "Step 1: Create test user"

# Register a test user via Synapse shared secret registration or login
# Try to register first, ignore if already exists
TEST_USER="mmtest_$(date +%s)"
REGISTER_RESULT=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/register" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"$TEST_USER\",\"password\":\"mmtest123\",\"auth\":{\"type\":\"m.login.dummy\"}}" 2>&1 || true)

# Login to get access token
LOGIN_RESULT=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/login" \
  -H "Content-Type: application/json" \
  -d "{\"type\":\"m.login.password\",\"user\":\"$TEST_USER\",\"password\":\"mmtest123\"}")

ACCESS_TOKEN=$(echo "$LOGIN_RESULT" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)
check "Login successful" "$ACCESS_TOKEN" "."
echo "    Token: ${ACCESS_TOKEN:0:20}..."

echo ""

# --- Step 2: Create a Matrix room ---
echo "Step 2: Create Matrix room"

ROOM_RESULT=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/createRoom" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"MM Test Room","preset":"public_chat"}')

ROOM_ID=$(echo "$ROOM_RESULT" | grep -o '"room_id":"[^"]*"' | cut -d'"' -f4)
check "Room created" "$ROOM_ID" "!"
echo "    Room: $ROOM_ID"

echo ""

# --- Step 3: Get OpenID token ---
echo "Step 3: Get OpenID token"

USER_ID=$(echo "$LOGIN_RESULT" | grep -o '"user_id":"[^"]*"' | cut -d'"' -f4)
OPENID_RESULT=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/user/$USER_ID/openid/request_token" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{}')

OPENID_TOKEN=$(echo "$OPENID_RESULT" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)
check "OpenID token obtained" "$OPENID_TOKEN" "."

echo ""

# --- Step 4: Exchange OpenID for MM JWT ---
echo "Step 4: Authenticate with MatrixMedia"

AUTH_RESULT=$(curl -sf -X POST "$MM_URL/_mm/client/v1/auth/token" \
  -H "Content-Type: application/json" \
  -d "{\"openid_token\":{\"access_token\":\"$OPENID_TOKEN\",\"token_type\":\"Bearer\",\"matrix_server_name\":\"localhost\",\"expires_in\":3600}}")

MM_TOKEN=$(echo "$AUTH_RESULT" | grep -o '"mm_token":"[^"]*"' | cut -d'"' -f4)
check "MM JWT obtained" "$MM_TOKEN" "."
echo "    User: $(echo "$AUTH_RESULT" | grep -o '"user_id":"[^"]*"' | cut -d'"' -f4)"

echo ""

# --- Step 5: Create a stream ---
echo "Step 5: Create audio stream"

# URL-encode the room_id for JSON (the ! character)
STREAM_RESULT=$(curl -sf -X POST "$MM_URL/_mm/client/v1/streams" \
  -H "Authorization: Bearer $MM_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"room_id\":\"$ROOM_ID\",\"media_type\":\"audio\",\"title\":\"E2E Test Stream\"}")

STREAM_ID=$(echo "$STREAM_RESULT" | grep -o '"stream_id":"[^"]*"' | cut -d'"' -f4)
SFU_URL=$(echo "$STREAM_RESULT" | grep -o '"sfu_url":"[^"]*"' | cut -d'"' -f4)
check "Stream created" "$STREAM_ID" "."
check "SFU URL returned" "$SFU_URL" "."
echo "    Stream: $STREAM_ID"

echo ""

# --- Step 6: Get stream details ---
echo "Step 6: Verify stream details"

DETAILS=$(curl -sf "$MM_URL/_mm/client/v1/streams/$STREAM_ID" \
  -H "Authorization: Bearer $MM_TOKEN")

check "Stream status active" "$DETAILS" '"status":"active"'
check "Stream has title" "$DETAILS" '"title":"E2E Test Stream"'
check "Host is mmtest" "$DETAILS" '"host_user_id"'

echo ""

# --- Step 7: List room streams ---
echo "Step 7: List room streams"

# URL-encode room_id for query param
ENCODED_ROOM=$(echo "$ROOM_ID" | sed 's/!/%21/g' | sed 's/:/%3A/g')
LIST_RESULT=$(curl -sf "$MM_URL/_mm/client/v1/rooms/$ENCODED_ROOM/streams" \
  -H "Authorization: Bearer $MM_TOKEN")

check "Stream listed in room" "$LIST_RESULT" "$STREAM_ID"

echo ""

# --- Step 8: Join stream (simulate a viewer) ---
echo "Step 8: Join stream as viewer"

# Create a second user for the viewer
VIEWER_USER="mmviewer_$(date +%s)"
curl -sf -X POST "$HS_URL/_matrix/client/v3/register" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"$VIEWER_USER\",\"password\":\"mmviewer123\",\"auth\":{\"type\":\"m.login.dummy\"}}" 2>&1 || true

VIEWER_LOGIN=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/login" \
  -H "Content-Type: application/json" \
  -d "{\"type\":\"m.login.password\",\"user\":\"$VIEWER_USER\",\"password\":\"mmviewer123\"}")

VIEWER_TOKEN=$(echo "$VIEWER_LOGIN" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)
VIEWER_UID=$(echo "$VIEWER_LOGIN" | grep -o '"user_id":"[^"]*"' | cut -d'"' -f4)

# Join the Matrix room first
curl -sf -X POST "$HS_URL/_matrix/client/v3/join/$ROOM_ID" \
  -H "Authorization: Bearer $VIEWER_TOKEN" > /dev/null 2>&1 || true

# Get viewer OpenID token
VIEWER_OPENID=$(curl -sf -X POST "$HS_URL/_matrix/client/v3/user/$VIEWER_UID/openid/request_token" \
  -H "Authorization: Bearer $VIEWER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{}')

VIEWER_OPENID_TOKEN=$(echo "$VIEWER_OPENID" | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)

# Get viewer MM JWT
VIEWER_AUTH=$(curl -sf -X POST "$MM_URL/_mm/client/v1/auth/token" \
  -H "Content-Type: application/json" \
  -d "{\"openid_token\":{\"access_token\":\"$VIEWER_OPENID_TOKEN\",\"token_type\":\"Bearer\",\"matrix_server_name\":\"localhost\",\"expires_in\":3600}}")

VIEWER_MM_TOKEN=$(echo "$VIEWER_AUTH" | grep -o '"mm_token":"[^"]*"' | cut -d'"' -f4)

# Join stream
JOIN_RESULT=$(curl -sf -X POST "$MM_URL/_mm/client/v1/streams/$STREAM_ID/join" \
  -H "Authorization: Bearer $VIEWER_MM_TOKEN" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: $(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || echo join-test-1)" \
  -d '{}')

check "Viewer joined" "$JOIN_RESULT" '"sfu_token"'

echo ""

# --- Step 9: List participants ---
echo "Step 9: Verify participants"

PARTS=$(curl -sf "$MM_URL/_mm/client/v1/streams/$STREAM_ID/participants" \
  -H "Authorization: Bearer $MM_TOKEN")

check "Has participants" "$PARTS" '"participants"'

echo ""

# --- Step 10: Viewer leaves ---
echo "Step 10: Viewer leaves stream"

LEAVE_RESULT=$(curl -sf -X POST "$MM_URL/_mm/client/v1/streams/$STREAM_ID/leave" \
  -H "Authorization: Bearer $VIEWER_MM_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{}')

check "Viewer left" "$LEAVE_RESULT" '"ok"'

echo ""

# --- Step 11: End stream ---
echo "Step 11: Host ends stream"

END_RESULT=$(curl -sf -X POST "$MM_URL/_mm/client/v1/streams/$STREAM_ID/end" \
  -H "Authorization: Bearer $MM_TOKEN" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: $(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || echo end-test-1)" \
  -d '{}')

check "Stream ended" "$END_RESULT" '"ok"'

# Verify stream is ended
ENDED_DETAILS=$(curl -sf "$MM_URL/_mm/client/v1/streams/$STREAM_ID" \
  -H "Authorization: Bearer $MM_TOKEN")

check "Status is ended" "$ENDED_DETAILS" '"ended"'

echo ""

# --- Step 12: Admin checks ---
echo "Step 12: Admin API verification"

HEALTH=$(curl -sf "$ADMIN_URL/_mm/admin/v1/health" \
  -H "Authorization: Bearer $ADMIN_TOKEN")

check "Admin health OK" "$HEALTH" '"status"'

STATS=$(curl -sf "$ADMIN_URL/_mm/admin/v1/stats" \
  -H "Authorization: Bearer $ADMIN_TOKEN")

check "Admin stats OK" "$STATS" "{"

echo ""

# --- Step 13: Metrics ---
echo "Step 13: Prometheus metrics"

METRICS=$(curl -sf "http://localhost:9090/metrics" 2>&1 || echo "UNREACHABLE")
check "Metrics endpoint reachable" "$METRICS" "mm_"

echo ""

# --- Summary ---
echo "================================="
echo "Results: $PASS passed, $FAIL failed, $TOTAL total"
if [ $FAIL -eq 0 ]; then
    echo -e "${GREEN}ALL TESTS PASSED${NC}"
    exit 0
else
    echo -e "${RED}$FAIL TESTS FAILED${NC}"
    exit 1
fi
