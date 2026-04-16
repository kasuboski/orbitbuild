#!/usr/bin/env bash
# integration-test.sh — Run a 3-node OrbitBuild integration test.
#
# Spins up Station + Satellite + Mission Control using --data-dir,
# verifies that MC discovers the satellite and creates a Unix socket bridge,
# then cleans everything up.
#
# Exits 0 on success, 1 on failure.
#
# Usage:
#   mise run integration-test
#   ./scripts/integration-test.sh
#   ./scripts/integration-test.sh --verbose   # show all output from nodes

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="$PROJECT_DIR/target/debug/orbitbuild"
TEST_DIR="/tmp/orbitbuild-integration-test-$$"
VERBOSE=false

if [[ "${1:-}" == "--verbose" ]]; then
    VERBOSE=true
fi

# --- helpers ---

info()  { printf '\033[0;32m[TEST]\033[0m %s\n' "$*"; }
warn()  { printf '\033[0;33m[TEST]\033[0m %s\n' "$*" >&2; }
fail()  { printf '\033[0;31m[FAIL]\033[0m %s\n' "$*" >&2; }

PIDS=()

cleanup() {
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    sleep 1
    for pid in "${PIDS[@]}"; do
        kill -9 "$pid" 2>/dev/null || true
    done
    rm -f /tmp/orbit-*.sock 2>/dev/null || true
    rm -rf "$TEST_DIR" 2>/dev/null || true
    info "cleaned up"
}

trap cleanup EXIT

# --- build ---

info "building orbitbuild..."
cd "$PROJECT_DIR"
if command -v nix &>/dev/null && [[ -f flake.nix ]]; then
    nix develop --command cargo build 2>&1 | tail -3
else
    cargo build 2>&1 | tail -3
fi

if [[ ! -x "$BINARY" ]]; then
    fail "binary not found at $BINARY"
    exit 1
fi

# --- setup ---

mkdir -p "$TEST_DIR"/{station,satellite,mc}
info "test dir: $TEST_DIR"

# --- step 1: station init ---

info "starting station..."
"$BINARY" --data-dir "$TEST_DIR/station" station init \
    > "$TEST_DIR/station.log" 2>&1 &
PIDS+=($!)

# Wait for beacon to appear in output
sleep 8

BEACON=$(grep '^ORBIT_BEACON=' "$TEST_DIR/station.log" | head -1 | sed 's/^ORBIT_BEACON=//' | tr -d '\n\r')
if [[ -z "$BEACON" ]]; then
    fail "no beacon found in station output"
    cat "$TEST_DIR/station.log"
    exit 1
fi
info "beacon: ${BEACON:0:50}..."

# --- step 2: satellite join ---

info "starting satellite..."
"$BINARY" --data-dir "$TEST_DIR/satellite" satellite join --beacon "$BEACON" \
    > "$TEST_DIR/satellite.log" 2>&1 &
PIDS+=($!)

sleep 8

if ! grep -q 'registered in doc' "$TEST_DIR/satellite.log" 2>/dev/null; then
    fail "satellite did not register"
    cat "$TEST_DIR/satellite.log"
    exit 1
fi
info "satellite registered"

# --- step 3: mission control ---

info "starting mission control..."
RUST_LOG=orbitbuild=debug \
    "$BINARY" --data-dir "$TEST_DIR/mc" mission-control --beacon "$BEACON" --platforms linux/amd64 \
    > "$TEST_DIR/mc.log" 2>&1 &
MC_PID=$!
PIDS+=($MC_PID)

# --- step 4: wait for socket ---

info "waiting for socket bridge..."
elapsed=0
while [[ $elapsed -lt 25 ]]; do
    if [[ -S /tmp/orbit-amd64.sock ]]; then
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

if [[ ! -S /tmp/orbit-amd64.sock ]]; then
    fail "socket /tmp/orbit-amd64.sock was not created within 25s"
    echo "--- MC output ---"
    cat "$TEST_DIR/mc.log"
    exit 1
fi
info "socket created: /tmp/orbit-amd64.sock"

# Verify MC is still running
if ! kill -0 "$MC_PID" 2>/dev/null; then
    warn "MC process exited (expected to stay running)"
    echo "--- MC output ---"
    cat "$TEST_DIR/mc.log"
fi

# --- step 5: verify `status` command ---

info "checking status command..."
STATUS_OUT=$("$BINARY" status --platforms linux/amd64 2>&1) || true
if echo "$STATUS_OUT" | grep -q 'Ready'; then
    info "status: socket reported Ready ✓"
else
    fail "status: did not report Ready"
    echo "$STATUS_OUT"
    exit 1
fi

# --- step 6: verify `status --wait` command ---

info "checking status --wait command..."
WAIT_OUT=$("$BINARY" status --wait --timeout-secs 5 --platforms linux/amd64 2>&1) || true
if echo "$WAIT_OUT" | grep -q 'Ready'; then
    info "status --wait: socket reported Ready ✓"
else
    fail "status --wait: did not report Ready"
    echo "$WAIT_OUT"
    exit 1
fi

# --- step 7: verify `fleet` command ---

mkdir -p "$TEST_DIR/fleet"
info "checking fleet command..."
FLEET_OUT=$("$BINARY" --data-dir "$TEST_DIR/fleet" fleet --beacon "$BEACON" 2>&1) || true
if echo "$FLEET_OUT" | grep -qE 'amd64.*idle'; then
    info "fleet: satellite discovered in doc ✓"
else
    fail "fleet: satellite not found in output"
    echo "$FLEET_OUT"
    exit 1
fi

# --- summary ---

echo ""
info "=== INTEGRATION TEST PASSED ==="
echo "  PIDs:           ${PIDS[*]}"
echo "  Socket:         /tmp/orbit-amd64.sock"
echo "  Test dir:       $TEST_DIR"
echo "  Checks:         socket, status, status --wait, fleet"
echo ""

if $VERBOSE; then
    echo "--- Station ---"
    cat "$TEST_DIR/station.log"
    echo ""
    echo "--- Satellite ---"
    cat "$TEST_DIR/satellite.log"
    echo ""
    echo "--- Mission Control ---"
    cat "$TEST_DIR/mc.log"
    echo ""
    echo "--- Status ---"
    echo "$STATUS_OUT"
    echo ""
    echo "--- Status --wait ---"
    echo "$WAIT_OUT"
    echo ""
    echo "--- Fleet ---"
    echo "$FLEET_OUT"
fi

info "all checks passed"
info "processes will be cleaned up on exit"
