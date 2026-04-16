# OrbitBuild Architecture

## Overview

OrbitBuild is a decentralized Docker build farm. A **Constellation** is the private P2P mesh of nodes that cooperate to run native multi-architecture builds. Every node speaks through the Iroh networking stack.

There are three node roles:

| Role | Responsibility |
|---|---|
| **Station** | Persistent seed. Creates the Constellation, bootstraps new peers. |
| **Satellite** | Compute runner. Hosts a local `buildkitd`, registers itself in the Doc, accepts proxied build sessions. |
| **Mission Control** | Ephemeral bridge (runs in CI or on a developer's machine). Discovers Satellites from the Doc and proxies local Unix sockets to remote buildkitd instances. |

A single cryptographic string — the **Beacon** — grants membership in the Constellation.

---

## Foundation Layers

### Iroh Endpoint

Every OrbitBuild node creates an `iroh::Endpoint` (QUIC over Magicsock). This provides:

- **Hole-punching** through NATs and firewalls (no VPN needed).
- **Relay fallback** via the n0 relay infrastructure.
- **TLS 1.3** encryption on all connections (ed25519 identities).
- **ALPN-based routing** through `iroh::protocol::Router`.

Each node has a permanent ed25519 `SecretKey` persisted to disk (`~/.orbitbuild/node.key`). The corresponding `PublicKey` is the node's identity across restarts.

### Iroh Router

All incoming connections are handled by a single `iroh::protocol::Router` per node. Protocols register under distinct ALPNs:

```
Router::builder(endpoint)
    .accept(BLOBS_ALPN, blobs_protocol)
    .accept(GOSSIP_ALPN, gossip_protocol)
    .accept(DOCS_ALPN, docs_protocol)
    .accept(ORBIT_BUILD_ALPN, build_proxy_handler)  // Satellites only
    .spawn()
```

The Router owns the accept loop, graceful shutdown, cancellation, and error handling. We never manage `endpoint.accept()` directly.

---

## Shared State: The Constellation Doc

### What it is

A single `iroh-docs` **Document** (a CRDT-based replicated KV store) is the beating heart of the Constellation. Every Station, Satellite, and Mission Control node holds a replica. Changes made by any node automatically sync to all others via the gossip/sync protocol.

### How it is created

`station init` creates the Doc:

```rust
let docs = Docs::memory().spawn(endpoint.clone(), blobs, gossip).await?;
let doc = docs.create().await?;                    // new namespace + author
let ticket = doc.share(ShareMode::Write, AddrInfoOptions::Relay).await?;
```

The `DocTicket` contains:
- The **namespace capability** (read + write keys for the Doc).
- The Station's **EndpointAddr** (public key + relay address).

### What lives in the Doc

Entries use **path-structured keys** with a common prefix:

| Key pattern | Value (JSON) | Written by |
|---|---|---|
| `satellite/<node_id_hex>` | `{ native_arch, platforms, status, endpoint_addr, registered_at }` | Satellite |

**Example satellite entry:**

```json
{
  "native_arch": "arm64",
  "platforms": ["linux/arm64"],
  "status": "idle",
  "endpoint_addr": "relay:https://relay.iroh.network/...",
  "registered_at": 1713123456
}
```

### How nodes join the Doc

Every joining node receives the Beacon, which wraps a `DocTicket`. The join flow:

```rust
let beacon: Beacon = beacon_string.parse()?;
let docs = Docs::memory().spawn(endpoint.clone(), blobs, gossip).await?;
let (doc, events) = docs.import_and_subscribe(beacon.doc_ticket()).await?;
```

`import_and_subscribe` does three things atomically:
1. Imports the Doc namespace using the capability from the ticket.
2. Starts syncing with the peers listed in the ticket (the Station).
3. Opens a subscription stream of `LiveEvent`s so the node sees all future changes.

From this point on, the local replica stays in sync. Every `doc.get_many(...)` is a **local read** — no round-trip to the Station required.

### Reading satellite state

```rust
let entries = doc.get_many(Query::prefix(b"satellite/")).await?;
```

### Reacting to changes

```rust
let events = doc.subscribe().await?;
while let Some(event) = events.next().await {
    match event? {
        LiveEvent::InsertRemote { entry, .. } => {
            // A satellite registered or updated its status
        }
        LiveEvent::DeleteRemote { .. } => {
            // A satellite went offline
        }
        _ => {}
    }
}
```

Mission Control uses this subscription to detect when new Satellites appear or existing ones change status, without polling.

---

## The Beacon

The Beacon is a single string that encodes everything a new node needs to join the Constellation:

**Wire format:** `orbit-v1-<base64url(postcard(DocTicket))>`

The Beacon wraps a `DocTicket`, which carries:
- The **namespace capability** (read/write keys) — this is the membership credential.
- The Station's **EndpointAddr** — the bootstrap peer for initial sync.

**Authorization is derived from membership.** Anyone holding the Beacon can import the Doc, sync state, and connect to Satellites. The DocTicket's embedded capability keys are the single gate — there is no separate shared secret.

The Beacon is safe to store as a GitHub Actions secret or pass as an environment variable (`ORBIT_BEACON`).

---

## Build Proxy Stream

The Doc handles *state* (who's available). A separate ALPN protocol handles the *data path* — tunneling buildkitd's Unix socket traffic between Mission Control and a Satellite.

### ALPN: `ORBITBUILD/BUILD/0`

This is the only custom `ProtocolHandler` we implement. It is a pure bidirectional byte pipe — no application-level handshake. Authentication is handled by the Router layer (see Authorization below).

### Connection flow

```
Mission Control                              Satellite
─────────────                               ─────────
                                            Router listening on BUILD_ALPN
                                            (gated by AccessLimit)

1. Select satellite from Doc
2. endpoint.connect(satellite_addr, BUILD_ALPN)
3. open_bi()  ──────────────────────────►   Router dispatches to BuildProxy
                                            BuildProxy::accept():
4. copy_bidirectional  ◄────────────────►     connect to local buildkitd Unix socket
   between Unix socket                        copy_bidirectional between
   and QUIC stream                             QUIC stream ↔ Unix socket
```

Both sides run `tokio::io::copy_bidirectional` to pipe bytes in both directions. The iroh QUIC stream handles congestion control, multiplexing, and encryption.

### Authorization

Possession of the Beacon (DocTicket) is the sole authorization mechanism. Any node that can import the Doc is a Constellation member and may connect to Satellites' buildkitd instances. There is no additional access control layer — the Beacon is the single gate.

```rust
let build_proxy = BuildProxy::new(buildkitd_socket);

Router::builder(endpoint)
    .accept(BLOBS_ALPN, blobs)
    .accept(GOSSIP_ALPN, gossip)
    .accept(DOCS_ALPN, docs)
    .accept(BUILD_ALPN, build_proxy)
    .spawn()
```

This means:

- To connect to a Satellite's buildkitd, you must be a Constellation member.
- To be a Constellation member, you must hold the Beacon (DocTicket).
- **The Beacon is the single gate.** No second secret, no HMAC, no application-level auth handshake.

### BuildProxy ProtocolHandler

```rust
#[derive(Debug, Clone)]
struct BuildProxy {
    buildkitd_socket: PathBuf,
}

impl ProtocolHandler for BuildProxy {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut quic_send, mut quic_recv) = connection.accept_bi().await?;
        let unix = UnixStream::connect(&self.buildkitd_socket).await?;
        let (mut unix_read, mut unix_write) = tokio::io::split(unix);
        tokio::try_join!(
            tokio::io::copy(&mut unix_read, &mut quic_send),
            tokio::io::copy(&mut quic_recv, &mut unix_write),
        )?;
        Ok(())
    }
}
```

---

## Node Lifecycles

### Station (`orbitbuild station init`)

```
1. Generate/load node identity (SecretKey → ~/.orbitbuild/node.key)
2. Create iroh Endpoint (presets::N0)
3. Create Blobs protocol (in-memory store)
4. Create Gossip protocol
5. Create Docs protocol
6. Create a new Doc
7. Share Doc → get DocTicket
8. Wrap DocTicket as Beacon, print ORBIT_BEACON=... to stdout
9. Spawn Router with (Blobs, Gossip, Docs) protocols
10. Wait for ctrl-c → Router::shutdown()
```

The Station does **not** register itself as a satellite. It is the seed node that other peers bootstrap from via the Beacon. Once a Satellite or Mission Control imports the Doc ticket and points at the Station as a sync peer, the iroh-docs engine handles all state replication automatically.

### Station Join (`orbitbuild station join --beacon <BEACON>`)

```
1. Generate/load node identity
2. Create iroh Endpoint
3. Parse Beacon → get DocTicket
4. Spawn Blobs + Gossip + Docs protocols
5. Import Doc from ticket (joins the replica set)
6. Spawn Router
7. Act as additional seed / sync peer
```

### Satellite (`orbitbuild satellite join --beacon <BEACON>`)

```
1. Generate/load node identity
2. Create iroh Endpoint
3. Parse Beacon → get DocTicket
4. Spawn Blobs + Gossip + Docs protocols
5. Import Doc from ticket → subscribe to live events
6. Write self-registration entry into Doc:
   key:   "satellite/<my_node_id_hex>"
   value: { native_arch, platforms, status: "idle", endpoint_addr, registered_at }
7. Spawn BuildProxy on Router
8. Start heartbeat: update Doc entry every 30s (or on status change)
9. Accept proxied buildkitd sessions via BuildProxy
10. On shutdown: set status to "offline" (or delete entry)
```

### Mission Control (`orbitbuild mission-control --beacon <BEACON>`)

```
1. Generate/load node identity
2. Create iroh Endpoint
3. Parse Beacon → get DocTicket
4. Spawn Blobs + Gossip + Docs protocols (no BuildProxy — MC never accepts builds)
5. Import Doc from ticket → subscribe to live events
6. Query Doc for satellites matching requested architectures
7. For each matched satellite:
   a. Create a local Unix domain socket (e.g., /tmp/orbit-arm64.sock)
   b. For each incoming connection on the Unix socket:
      - Connect to satellite via endpoint.connect(satellite_addr, BUILD_ALPN)
      - copy_bidirectional between Unix socket ↔ QUIC stream
8. Run `docker buildx create --name orbit --driver remote unix:///tmp/orbit-arm64.sock --platform linux/arm64`
9. Wait for ctrl-c → clean up sockets + remove buildx builder
```

### Status (`orbitbuild status --wait`)

```
1. Check if local Unix socket files exist (/tmp/orbit-*.sock)
2. Attempt to connect to each socket
3. If --wait: poll until all requested platforms have live sockets (or timeout)
4. Print status per platform
```

### Link (`orbitbuild link --beacon <BEACON>`)

```
1. Spawn mission-control daemon as background process
2. Run orbitbuild status --wait (blocking until ready)
3. Run docker buildx create commands
4. Print instructions
5. On ctrl-c: clean up buildx builder, kill background process
```

### Fleet (`orbitbuild fleet --beacon <BEACON>`)

```
1. Parse Beacon → import Doc
2. Read all satellite entries from Doc
3. Display table: node_id, arch, status, registered_at
```

---

## Data Flow Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│                        Constellation Doc                         │
│         (iroh-docs CRDT — replicated to all nodes)              │
│                                                                  │
│  ┌─────────────────────────────┐  ┌─────────────────────────────┐ │
│  │ satellite/abc123             │  │ satellite/def456             │ │
│  │ { native_arch: "arm64",     │  │ { native_arch: "amd64",     │ │
│  │   platforms: ["linux/arm64"],│  │   platforms: ["linux/amd64"],│ │
│  │   status: "idle",           │  │   status: "idle",           │ │
│  │   endpoint_addr: .. }       │  │   endpoint_addr: .. }       │ │
│  └─────────────────────────────┘  └─────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
          ▲ set_bytes()          ▲ set_bytes()          ▲ get_many()
          │                      │                      │ subscribe()
          │                      │                      │
    ┌─────┴──────┐        ┌─────┴──────┐        ┌─────┴──────────┐
    │ Satellite  │        │ Satellite  │        │ Mission Control│
    │ (arm64)    │        │ (amd64)    │        │                │
    │            │        │            │        │  /tmp/orbit-   │
    │ buildkitd  │        │ buildkitd  │        │  arm64.sock ──►├──── iroh QUIC ───► Satellite
    │ ↑          │        │ ↑          │        │  amd64.sock ──►├──── iroh QUIC ───► Satellite
    │ │  proxy   │        │ │  proxy   │        │                │
    │ BuildProxy │        │ BuildProxy │        │  docker buildx │
    └────────────┘        └────────────┘        └────────────────┘
```

---

## ALPN Protocols per Node Role

| ALPN | Station | Satellite | Mission Control |
|---|:---:|:---:|:---:|
| `/iroh-blobs/1` | ✔ | ✔ | ✔ |
| `/iroh-gossip/1` | ✔ | ✔ | ✔ |
| `/iroh-sync/1` | ✔ | ✔ | ✔ |
| `ORBITBUILD/BUILD/0` | — | ✔ (accept) | — (dial) |

All nodes run blobs + gossip + docs (required by iroh-docs). Only Satellites listen on the build ALPN, and only for known Doc members.

---

## Dependency Stack

```
orbitbuild
├── iroh                 — Endpoint, Router, ProtocolHandler, EndpointAddr
├── iroh-docs 0.97       — Docs, Doc, DocTicket, LiveEvent, Engine
├── iroh-blobs 0.99      — BlobsProtocol, MemStore (required by iroh-docs)
├── iroh-gossip 0.97     — Gossip (required by iroh-docs)
├── tokio                — async runtime, copy_bidirectional, Unix sockets
├── clap                 — CLI argument parsing
├── serde + serde_json   — serialization for Doc entries
├── data-encoding        — base64url Beacon encoding
├── postcard             — compact binary Beacon payload
├── tracing              — structured logging
└── anyhow               — error handling
```

---

## Module Map

| Module | Purpose |
|---|---|
| `beacon` | Beacon serialization/deserialization (wraps DocTicket) |
| `keys` | Node identity persistence (`load_or_generate_secret_key`) |
| `router` | Shared setup: spawn Router with Blobs + Gossip + Docs |
| `station` | Station lifecycle (init, join) |
| `satellite` | Satellite lifecycle (join, heartbeat, buildkitd supervision) |
| `build_proxy` | `ProtocolHandler` for `ORBITBUILD/BUILD/0` ALPN |
| `mission_control` | Mission Control daemon (discover, proxy, buildx integration) |
| `bridge` | Unix socket ↔ satellite tunnel management |
| `docker` | Docker buildx CLI integration |
| `status` | Status command (check local socket health) |
| `link` | Link command (background daemon + buildx setup) |
| `fleet` | Fleet command (read Doc, format table) |
| `cli` | Clap derive CLI definitions |
| `lib` | Module re-exports |

---

## Key Design Decisions

1. **iroh-docs for state, custom protocol only for data.** The Doc CRDT handles satellite registration, discovery, and state changes. The only custom protocol is the bidirectional build proxy.

2. **Router manages all accept loops.** We never call `endpoint.accept()` directly. Each protocol is a `ProtocolHandler` registered on the Router.

3. **Beacon is a DocTicket.** Membership in the Constellation is the sole authorization mechanism. The DocTicket's embedded capability keys control who can read and write the Doc. No separate shared secret.

4. **Authorization through Beacon possession.** Possessing the Beacon (DocTicket) is the sole authorization mechanism. Any node that can import the Doc is a Constellation member and may connect to Satellites.

5. **Local reads for discovery.** Mission Control reads satellite state from its local Doc replica — no round-trip to the Station. The Station can go offline after initial sync and existing peers continue to discover each other through gossip.

6. **No application-level auth handshake.** iroh provides transport-layer authentication (ed25519 identities) and encryption (TLS 1.3). The build proxy is a pure byte pipe with no framing beyond QUIC streams.

7. **Sans-IO core logic.** Beacon serialization, Doc entry parsing, and arch detection are all testable without networking. Only the `ProtocolHandler::accept` implementations and `endpoint.connect()` calls touch the network.
