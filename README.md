# OrbitBuild

**Decentralized, peer-to-peer Docker build orchestration.**

Turn a fleet of native arm64 and amd64 machines into a unified build farm — no QEMU emulation, no VPNs, no SSH tunnels. OrbitBuild uses the [Iroh](https://iroh.computer) P2P protocol for zero-config connectivity across firewalls and NATs, managed by a single cryptographic string called a **Beacon**.

---

## How It Works

```
                          ┌──────────────────────────────┐
                          │      Constellation Doc       │
                          │   (CRDT — replicated to all) │
                          │                              │
                          │  satellite/abc → { arm64 }   │
                          │  satellite/def → { amd64 }   │
                          └──────────┬───────────────────┘
                                     │
              ┌──────────────────────┼──────────────────────┐
              │                      │                      │
     ┌────────┴────────┐   ┌─────────┴───────┐   ┌──────────┴────────┐
     │    Station      │   │   Satellite     │   │  Mission Control  │
     │  (seed node)    │   │  (build runner) │   │  (local bridge)   │
     │                 │   │                 │   │                   │
     │  Holds Doc,     │   │  Runs buildkitd │   │  /tmp/orbit-      │
     │  bootstraps     │   │  registers in   │   │   arm64.sock ─────┼── QUIC ──► arm64 Satellite
     │  new peers      │   │  Doc, accepts   │   │   amd64.sock ─────┼── QUIC ──► amd64 Satellite
     │                 │   │  proxied builds │   │                   │
     └─────────────────┘   └─────────────────┘   │  docker buildx    │
                                                 └───────────────────┘
```

1. **Station init** creates the Constellation and outputs a Beacon.
2. **Satellites** join with the Beacon, register in the shared Doc, and serve their local buildkitd over QUIC.
3. **Mission Control** discovers Satellites from the Doc and bridges local Unix sockets to remote buildkitd instances.
4. **`docker buildx build`** targets the local sockets — builds run at native speed on remote hardware.

All traffic is encrypted end-to-end via TLS 1.3. Authorization is gated by the Beacon (a `DocTicket` containing cryptographic capability keys).

---

## Quick Start

### 1. Create a Constellation

```bash
# On your seed machine (e.g. a cloud VM or always-on server)
orbitbuild station init
# Outputs: ORBIT_BEACON=orbit-v1-<base64-encoded-ticket>
```

### 2. Join Satellites

```bash
# On each build machine (native arm64, amd64, etc.)
orbitbuild satellite join --beacon "$ORBIT_BEACON"
```

### 3. Link and Build

```bash
# On your dev machine or CI runner
orbitbuild link --beacon "$ORBIT_BEACON"

# Build natively across architectures
docker buildx build \
  --builder orbit \
  --platform linux/amd64,linux/arm64 \
  -t myapp:latest .
```

---

## Installation

### Pre-built Binary

Download the latest release from [GitHub Releases](https://github.com/kasuboski/orbitbuild/releases).

### From Source

```bash
git clone https://github.com/kasuboski/orbitbuild.git
cd orbitbuild
cargo install --path .
```

### Nix

```bash
nix develop    # enter dev shell (provides gcc, pkg-config, openssl)
cargo build    # then build as normal
```

---

## CLI Reference

| Command | Description |
|:--------|:------------|
| `station init` | Generate a new Constellation and output the Beacon. |
| `station join --beacon <BEACON>` | Join an existing Constellation as a redundant seed node. |
| `satellite join --beacon <BEACON>` | Start the builder agent and register with the Constellation. |
| `satellite join --beacon <BEACON> --platforms linux/arm64` | Advertise additional cross-arch platforms (requires QEMU/binfmt). |
| `mission-control --beacon <BEACON>` | Run the P2P bridge daemon (links local sockets → remote Satellites). |
| `status [--wait] [--timeout-secs 30]` | Check local bridge health. `--wait` blocks until ready. |
| `link --beacon <BEACON>` | One-shot: start Mission Control, wait for readiness, configure buildx. |
| `fleet --beacon <BEACON>` | Display a table of all active Satellites, their arch, and status. |

All `--beacon` flags also read from the `ORBIT_BEACON` environment variable.  
Data directory defaults to `$HOME/.orbitbuild` (override with `--data-dir` or `ORBIT_DATA_DIR`).

---

## GitHub Action

Use OrbitBuild in your CI pipelines:

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Set up OrbitBuild
        uses: kasuboski/orbitbuild/setup-action@v1
        with:
          beacon: ${{ secrets.ORBIT_BEACON }}
          platforms: linux/amd64,linux/arm64

      - name: Build multi-arch image
        run: |
          docker buildx build \
            --builder orbit \
            --platform linux/amd64,linux/arm64 \
            -t myapp:${{ github.sha }} \
            .
```

**Action inputs:**

| Input | Default | Description |
|:------|:--------|:------------|
| `beacon` | *(required)* | The `ORBIT_BEACON` secret. |
| `platforms` | `linux/amd64,linux/arm64` | Comma-separated architectures to link. |
| `version` | `latest` | OrbitBuild binary version to download. |

The Action downloads the OrbitBuild binary, starts Mission Control in the background, waits for readiness, and configures `docker buildx`. Cleanup runs automatically when the job ends.

---

## Development

### Prerequisites

- [mise](https://mise.jdx.dev) for task running and toolchain management
- Docker (for integration and build tests)

```bash
mise install          # install Rust and tools
mise run build        # cargo build
mise run check        # cargo check (fast)
```

### Tasks

| Task | Command | Description |
|:-----|:--------|:------------|
| Build | `mise run build` | `cargo build` |
| Test | `mise run test` | `cargo test` (61 unit tests) |
| Lint | `mise run lint` | `cargo clippy` + `actionlint` + action TS lint |
| Check | `mise run check` | Quick compilation check |
| Integration | `mise run integration-test` | 3-node P2P test (station + satellite + MC) |
| Build E2E | `mise run build-test` | Full build through P2P tunnel (requires Docker) |

### Testing

```bash
mise run test                # unit tests
mise run integration-test    # P2P discovery, status, fleet
mise run build-test          # full Docker build through P2P tunnel
```

### Logging

OrbitBuild uses `tracing` with `RUST_LOG`:

```bash
RUST_LOG=orbitbuild=debug orbitbuild satellite join --beacon "$ORBIT_BEACON"
```

---

## Architecture

OrbitBuild has three node roles connected by a single `iroh-docs` CRDT Document:

- **Station** — persistent seed node that creates the Constellation and bootstraps peers.
- **Satellite** — compute runner hosting a local buildkitd, registered in the Doc, accepting proxied build sessions.
- **Mission Control** — ephemeral bridge (CI/dev) that discovers Satellites from the Doc and proxies local Unix sockets to remote buildkitd instances via QUIC.

The **Beacon** wraps a `DocTicket` (namespace capability + bootstrap peer address). Possessing it is the sole authorization mechanism — no separate secrets or handshakes.

> For the full technical architecture, see [ARCH.md](ARCH.md).  
> For product requirements, see [SPEC.md](SPEC.md).

---

## License

Licensed under either of [Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0) or [MIT license](http://opensource.org/licenses/MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
