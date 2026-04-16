use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// OrbitBuild — Decentralized, peer-to-peer Docker build orchestration.
///
/// Use a Constellation of native arm64 and amd64 machines as a unified
/// high-performance build farm, bypassing QEMU emulation and VPN complexity.
#[derive(Parser, Debug)]
#[command(name = "orbitbuild", version, arg_required_else_help = true)]
pub struct Cli {
    /// Directory for node identity and persistent data.
    ///
    /// Defaults to $HOME/.orbitbuild if not specified.
    #[clap(long, global = true, env = "ORBIT_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    #[clap(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage the Constellation's Station (seed node).
    #[command(subcommand)]
    Station(StationCommand),

    /// Manage Satellite compute nodes (buildkitd runners).
    #[command(subcommand)]
    Satellite(SatelliteCommand),

    /// Run the Mission Control daemon (P2P bridge to Constellation).
    MissionControl {
        /// The ORBIT_BEACON secret (or set ORBIT_BEACON env var).
        #[clap(long, env = "ORBIT_BEACON")]
        beacon: String,

        /// Comma-separated architectures to link.
        #[clap(long, default_value = "linux/amd64,linux/arm64")]
        platforms: String,
    },

    /// Check local bridge / Mission Control health.
    Status {
        /// Wait until the bridge is ready (with timeout).
        #[clap(long)]
        wait: bool,

        /// Timeout in seconds when using --wait.
        #[clap(long, default_value = "30")]
        timeout_secs: u64,

        /// Comma-separated architectures to wait for.
        #[clap(long, default_value = "linux/amd64,linux/arm64")]
        platforms: String,
    },

    /// Configure local docker buildx to use the Constellation.
    Link {
        /// The ORBIT_BEACON secret (or set ORBIT_BEACON env var).
        #[clap(long, env = "ORBIT_BEACON")]
        beacon: String,

        /// Comma-separated architectures to link.
        #[clap(long, default_value = "linux/amd64,linux/arm64")]
        platforms: String,
    },

    /// Display a table of all active Satellites, their arch, and latency.
    Fleet {
        /// The ORBIT_BEACON secret (or set ORBIT_BEACON env var).
        #[clap(long, env = "ORBIT_BEACON")]
        beacon: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum StationCommand {
    /// Generate a new Constellation and output the Beacon.
    Init,

    /// Join an existing Constellation as a redundant seed node.
    Join {
        /// The ORBIT_BEACON secret of the existing Constellation.
        #[clap(long, env = "ORBIT_BEACON")]
        beacon: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SatelliteCommand {
    /// Start the builder agent and register with the Constellation.
    Join {
        /// The ORBIT_BEACON secret of the Constellation to join.
        #[clap(long, env = "ORBIT_BEACON")]
        beacon: String,

        /// Path to the local buildkitd Unix socket.
        ///
        /// Defaults to /run/buildkit/buildkitd.sock.
        #[clap(long)]
        buildkitd_socket: Option<String>,

        /// Comma-separated platforms this satellite can build.
        ///
        /// The native platform is always included automatically.
        /// Add additional platforms if this satellite has QEMU/binfmt
        /// configured for cross-architecture builds.
        ///
        /// Example: --platforms linux/arm64 (on an amd64 host with QEMU)
        #[clap(long)]
        platforms: Option<String>,
    },
}
