# OrbitBuild Product Requirements Document (PRD)

## 1. Executive Summary
**OrbitBuild** is a decentralized, peer-to-peer Docker build orchestration platform. It allows organizations to utilize a "Constellation" of native `arm64` and `amd64` machines as a unified, high-performance build farm, bypassing the performance penalties of QEMU emulation and the configuration complexity of VPNs or SSH tunnels.

By utilizing the **Iroh P2P protocol**, OrbitBuild provides zero-config connectivity across firewalls and NATs, managed entirely through a single cryptographic string called a **Beacon**.

---

## 2. Core Terminology (The Orbit Metaphor)
*   **Constellation:** The private, encrypted P2P mesh network.
*   **Station:** Persistent seed nodes that maintain the Constellation’s state (the registry of active Satellites).
*   **Satellite:** Compute nodes (runners) that host `buildkitd` and execute the actual builds.
*   **Beacon:** A self-contained cryptographic ticket used for discovery and authentication.
*   **Mission Control:** The local CLI/GitHub Action that bridges the developer’s environment to the Constellation.

---

## 3. Technical Architecture

### 3.1 Frameworks & Languages
*   **Core Logic:** Rust (utilizing `tokio` for concurrency).
*   **Networking:** `iroh-net` (QUIC, Magicsock hole-punching) and `iroh-docs` (distributed KV store).
*   **Build Engine:** Native `buildkitd` (managed as a child process).
*   **Interface:** `docker buildx` CLI (via remote driver integration).

### 3.2 Security Model
*   **Identity:** Every node (Station, Satellite, Client) has a permanent ed25519 Public Key.
*   **Authentication:** Access to the Constellation is gated by the **Beacon**, which contains the Document Capability (read/write keys) for the Iroh Doc.
*   **Transport:** All traffic is encrypted end-to-end via TLS 1.3 (provided by Iroh/QUIC).

---

## 4. User Journeys & Experience

### 4.1 Infrastructure: Establishing the Constellation
The Admin creates the "Gravity Well" of the network.
1.  **Station Init:** `orbitbuild station init` generates a new Iroh Document locally.
2.  **State Persistence:** The Station saves the private keys and the "phonebook" of runners to a persistent volume.
3.  **Beacon Generation:** The Station outputs the `ORBIT_BEACON`. This string includes the Station's IP/Relay and the Document's Read/Write keys.

### 4.2 Compute: Deploying Satellites
Turning any native machine into a builder.
1.  **Deployment:** A user runs the `orbitbuild satellite join` binary on a native ARM64/AMD64 host.
2.  **Self-Registration:** The Satellite connects to the Station using the Beacon and writes its metadata into the Iroh Doc: `{ "node_id": "...", "arch": "arm64", "status": "idle", "ticket": "..." }`.
3.  **Supervision:** The Satellite manages a local `buildkitd` instance, ensuring it is healthy and listening on a local-only loopback.

### 4.3 CI/CD: The GitHub Action Mission
Automated multi-arch builds without native GitHub runners.
1.  **Background Bridge:** The `orbitbuild/setup-action` starts the `orbitbuild mission-control` daemon in the background.
2.  **Discovery:** The daemon finds a healthy Satellite for each requested platform in the Beacon’s phonebook.
3.  **Tunneling:** The daemon opens local Unix sockets (e.g., `/tmp/orbit-arm64.sock`) and pipes them via Iroh to the remote Satellites.
4.  **Docker Integration:** The action executes:
    `docker buildx create --name orbit --append --driver remote unix:///tmp/orbit-arm64.sock --platform linux/arm64`
5.  **Readiness:** The action calls `orbitbuild status --wait` to ensure the bridge is hot before the next step begins.

---

## 5. CLI Specification

| Command | Description |
| :--- | :--- |
| `station init` | Generates a new Constellation and outputs the Beacon. |
| `station join` | Joins an existing Constellation as a redundant Seed. |
| `satellite join` | Starts the builder agent and registers with the Beacon. |
| `mission-control` | (Internal/Daemon) Maintains the P2P bridge and local sockets. |
| `status` | Checks local bridge health. Supports `--wait` for CI scripts. |
| `link` | Configures the local `docker buildx` context to use the Constellation. |
| `fleet` | Displays a table of all active Satellites, their arch, and latency. |

---

## 6. GitHub Action Integration Details

### Action Input: `orbitbuild/setup-action@v1`
```yaml
inputs:
  beacon:
    description: "The ORBIT_BEACON secret"
    required: true
  platforms:
    description: "Comma-separated architectures to link"
    default: "linux/amd64,linux/arm64"
```

### The "Mission Control" Daemon Lifecycle:
1.  **Step 1:** Starts `orbitbuild mission-control --beacon ${{ inputs.beacon }}` in background.
2.  **Step 2:** Daemon performs Iroh sync and finds Satellites.
3.  **Step 3:** Daemon calls `docker buildx create` for each platform.
4.  **Step 4:** `orbitbuild status --wait` ensures the P2P hole-punching is successful.
5.  **Cleanup:** GitHub Actions automatically kills the background process at the end of the job, tearing down the P2P connections.

---

## 7. Performance & Optimization Goals
*   **Direct Connect:** 80%+ of connections should achieve P2P hole-punching (bypassing relays) for maximum throughput.
*   **Zero-Copy Proxy:** The Rust proxy will utilize `tokio::io::copy_bidirectional` for minimal CPU overhead during large layer transfers.
*   **Native Speed:** Multi-arch builds should perform at the native speed of the Satellite hardware, typically 5x-10x faster than QEMU-based builds on standard GitHub Runners.
*   **Discovery Latency:** The `status --wait` command should return "Ready" in < 2 seconds when using a persistent Station.

---

## 8. Success Metrics
*   **Developer Experience:** Time from "Beacon in hand" to "Successful multi-arch build" < 5 minutes.
*   **Reliability:** 99.9% build success rate when at least one Satellite per architecture is online.
*   **Portability:** Single-binary CLI with zero dynamic library dependencies (`musl` build for Linux).
