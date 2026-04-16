OrbitBuild is a decentralized, peer-to-peer Docker build orchestration platform. It allows organizations to utilize a "Constellation" of native arm64 and amd64 machines as a unified, high-performance build farm, bypassing the performance penalties of QEMU emulation and the configuration complexity of VPNs or SSH tunnels.

By utilizing the Iroh P2P protocol, OrbitBuild provides zero-config connectivity across firewalls and NATs, managed entirely through a single cryptographic string called a Beacon.

Frameworks & Languages

    Core Logic: Rust (utilizing tokio for concurrency).

    Networking: iroh-net (QUIC, Magicsock hole-punching) and iroh-docs (distributed KV store).

    Build Engine: Native buildkitd (managed as a child process).

    Interface: docker buildx CLI (via remote driver integration).

Security Model

    Identity: Every node (Station, Satellite, Client) has a permanent ed25519 Public Key.

    Authentication: Access to the Constellation is gated by the Beacon, which contains the Document Capability (read/write keys) for the Iroh Doc.

    Transport: All traffic is encrypted end-to-end via TLS 1.3 (provided by Iroh/QUIC).

Knowledge
    @SPEC.md outlines the general idea and UX. @ARCH.md outlines the technical architecture.
    There is a folder called knowledge/ that has information that may be helpful. Explore this folder when asking questions.
    In particular knowledge/repos has git clones of projects we depend on (iroh) and examples (dumbpipe)

Development Environment

    mise is the primary task runner and toolchain manager. Run `mise install` to install Rust and other tools.
    Available mise tasks: build, lint, test, check. Run with `mise run <task>`.
    Cargo.toml is in the repo root.

    A flake.nix is also checked in for NixOS users (or anyone preferring nix). It provides system dependencies that mise doesn't cover (gcc, pkg-config, openssl). On NixOS, enter the dev shell first: `nix develop`, then use mise/cargo as normal. Non-Nix users can ignore flake.nix entirely.

Development Process
    Always use mise for project dependencies.
    Create mise tasks for common usecases (build, lint, test).

Testing Methodology
    Unit tests are ok but ensure we are testing the actual logic and not underlying libraries
    Sans-io style code improves testability - e.g. Test our logic not the networking path
