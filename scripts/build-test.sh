#!/usr/bin/env bash
# build-test.sh — Full end-to-end build test.
#
# Spins up the complete OrbitBuild stack (station + satellite + mission-control)
# with a real buildkitd container, then runs `docker buildx build` through the
# P2P tunnel.
#
# Requirements:
#   - Docker daemon running
#   - orbitbuild binary built (runs `cargo build` if needed)
#
# Usage:
#   mise run build-test
#   ./scripts/build-test.sh
#   ./scripts/build-test.sh --verbose
#   ./scripts/build-test.sh --cleanup   # also remove buildkitd container after

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="$PROJECT_DIR/target/debug/orbitbuild"
TEST_DIR="/tmp/orbitbuild-build-test-$$"
BUILDKITD_SOCKET_DIR="/tmp/orbitbuild-build-test-$$/buildkitd"
BUILDKITD_CONTAINER="orbitbuild-buildkitd-test-$$"
VERBOSE=false
CLEANUP_ONLY=false

for arg in "$@"; do
    case "$arg" in
        --verbose)  VERBOSE=true ;;
        --cleanup)  CLEANUP_ONLY=true ;;
    esac
done

# --- helpers ---

info()  { printf '\033[0;32m[TEST]\033[0m %s\n' "$*"; }
warn()  { printf '\033[0;33m[TEST]\033[0m %s\n' "$*" >&2; }
fail()  { printf '\033[0;31m[FAIL]\033[0m %s\n' "$*" >&2; }

PIDS=()

cleanup() {
    # Kill orbitbuild processes
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    sleep 1
    for pid in "${PIDS[@]}"; do
        kill -9 "$pid" 2>/dev/null || true
    done

    # Remove buildx builder
    docker buildx rm orbit-build-test 2>/dev/null || true

    # Remove buildkitd container
    docker rm -f "$BUILDKITD_CONTAINER" 2>/dev/null || true

    # Clean up test dir and sockets
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

mkdir -p "$TEST_DIR"/{station,satellite,mc,buildkitd,build-context}
info "test dir: $TEST_DIR"

# --- step 0: create test Dockerfile ---

cat > "$TEST_DIR/build-context/Dockerfile" << 'DOCKERFILE'
FROM alpine:latest
RUN echo "Hello from OrbitBuild! Built via P2P tunnel at $(date)" > /hello
RUN cat /hello
CMD ["cat", "/hello"]
DOCKERFILE
info "test Dockerfile created"

# --- step 1: start buildkitd container ---

info "starting buildkitd container..."
docker run -d --name "$BUILDKITD_CONTAINER" \
    --privileged \
    -v "$BUILDKITD_SOCKET_DIR":/run/buildkit \
    moby/buildkit:latest \
    --addr unix:///run/buildkit/buildkitd.sock \
    > /dev/null 2>&1

# Wait for buildkitd to start
sleep 3

if ! docker ps --format '{{.Names}}' | grep -q "$BUILDKITD_CONTAINER"; then
    fail "buildkitd container failed to start"
    docker logs "$BUILDKITD_CONTAINER" 2>&1 | tail -10
    exit 1
fi

# Fix socket permissions
docker exec "$BUILDKITD_CONTAINER" chmod 666 /run/buildkit/buildkitd.sock 2>/dev/null || true

if [[ ! -S "$BUILDKITD_SOCKET_DIR/buildkitd.sock" ]]; then
    fail "buildkitd socket not found at $BUILDKITD_SOCKET_DIR/buildkitd.sock"
    exit 1
fi
info "buildkitd ready: $BUILDKITD_SOCKET_DIR/buildkitd.sock"

# --- step 2: station init ---

info "starting station..."
"$BINARY" --data-dir "$TEST_DIR/station" station init \
    > "$TEST_DIR/station.log" 2>&1 &
PIDS+=($!)
sleep 6

BEACON=$(grep '^ORBIT_BEACON=' "$TEST_DIR/station.log" | head -1 | sed 's/^ORBIT_BEACON=//' | tr -d '\n\r')
if [[ -z "$BEACON" ]]; then
    fail "no beacon found in station output"
    cat "$TEST_DIR/station.log"
    exit 1
fi
info "beacon: ${BEACON:0:50}..."

# --- step 3: satellite join ---

info "starting satellite..."
"$BINARY" --data-dir "$TEST_DIR/satellite" satellite join \
    --beacon "$BEACON" \
    --buildkitd-socket "$BUILDKITD_SOCKET_DIR/buildkitd.sock" \
    > "$TEST_DIR/satellite.log" 2>&1 &
PIDS+=($!)
sleep 8

if ! grep -q 'registered in doc' "$TEST_DIR/satellite.log" 2>/dev/null; then
    fail "satellite did not register"
    cat "$TEST_DIR/satellite.log"
    exit 1
fi
info "satellite registered"

# --- step 4: mission control ---

info "starting mission control..."
RUST_LOG=orbitbuild=debug \
    "$BINARY" --data-dir "$TEST_DIR/mc" mission-control \
    --beacon "$BEACON" \
    --platforms linux/amd64 \
    > "$TEST_DIR/mc.log" 2>&1 &
MC_PID=$!
PIDS+=($MC_PID)

# --- step 5: wait for socket ---

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
    fail "socket not created within 25s"
    cat "$TEST_DIR/mc.log"
    exit 1
fi
info "socket ready: /tmp/orbit-amd64.sock"

# --- step 6: verify status ---

STATUS_OUT=$("$BINARY" status --platforms linux/amd64 2>&1) || true
if echo "$STATUS_OUT" | grep -q 'Ready'; then
    info "status: Ready ✓"
else
    warn "status: not ready (proceeding anyway)"
fi

# --- step 7: verify fleet ---

mkdir -p "$TEST_DIR/fleet"
FLEET_OUT=$("$BINARY" --data-dir "$TEST_DIR/fleet" fleet --beacon "$BEACON" 2>&1) || true
if echo "$FLEET_OUT" | grep -qE 'amd64.*idle'; then
    info "fleet: satellite discovered ✓"
else
    warn "fleet: satellite not found (proceeding anyway)"
fi

# --- step 8: docker build! ---

docker buildx rm orbit-build-test 2>/dev/null || true
docker buildx create --name orbit-build-test --driver remote \
    unix:///tmp/orbit-amd64.sock 2>&1

info "building image through P2P tunnel..."
docker buildx build \
    --builder orbit-build-test \
    --platform linux/amd64 \
    -t orbitbuild-e2e-test:latest \
    "$TEST_DIR/build-context/" \
    2>&1

BUILD_EXIT=$?

echo ""

if [[ $BUILD_EXIT -eq 0 ]]; then
    info "=========================================="
    info "  BUILD SUCCEEDED via P2P tunnel"
    info "=========================================="
    info ""
    info "  docker → /tmp/orbit-amd64.sock → MC → QUIC → Satellite → buildkitd"
    info ""
else
    fail "BUILD FAILED (exit $BUILD_EXIT)"
    echo ""
    echo "--- MC log (last 20 lines) ---"
    tail -20 "$TEST_DIR/mc.log"
    echo ""
    echo "--- Satellite log (last 20 lines) ---"
    tail -20 "$TEST_DIR/satellite.log"
    exit 1
fi

if $VERBOSE; then
    echo ""
    echo "--- Station ---"
    cat "$TEST_DIR/station.log"
    echo ""
    echo "--- Satellite ---"
    cat "$TEST_DIR/satellite.log"
    echo ""
    echo "--- Mission Control ---"
    cat "$TEST_DIR/mc.log"
fi
