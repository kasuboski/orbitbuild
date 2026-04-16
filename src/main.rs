use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use orbitbuild::cli::{Cli, Commands, SatelliteCommand, StationCommand};
use orbitbuild::beacon::Beacon;
use orbitbuild::station;

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("orbitbuild=info".parse().unwrap()),
        )
        .init();
}

/// Resolve the data directory: CLI flag > env var > default ($HOME/.orbitbuild).
fn resolve_data_dir(cli_data_dir: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match cli_data_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            Ok(dir)
        }
        None => orbitbuild::keys::default_data_dir(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let data_dir = resolve_data_dir(cli.data_dir)?;

    match cli.command {
        Commands::Station(cmd) => match cmd {
            StationCommand::Init => {
                tracing::info!(data_dir = %data_dir.display(), "station init");
                station::run_station_init(&data_dir).await?;
            }
            StationCommand::Join { beacon } => {
                tracing::info!(beacon = %beacon, "station join");
                let beacon: Beacon = beacon.parse()?;
                station::run_station_join(&beacon, &data_dir).await?;
            }
        },
        Commands::Satellite(cmd) => match cmd {
            SatelliteCommand::Join { beacon, buildkitd_socket, platforms } => {
                tracing::info!(beacon = %beacon, "satellite join");
                let beacon: Beacon = beacon.parse()?;
                let buildkitd_socket = buildkitd_socket.map(std::path::PathBuf::from);
                let extra_platforms = platforms
                    .map(|p| orbitbuild::satellite::parse_satellite_platforms(&p))
                    .transpose()?;
                orbitbuild::satellite::run_satellite_join(beacon, &data_dir, buildkitd_socket, extra_platforms).await?;
            }
        },
        Commands::MissionControl { beacon, platforms } => {
            tracing::info!(beacon = %beacon, platforms = %platforms, "mission-control");
            let beacon: Beacon = beacon.parse()?;
            let platforms = orbitbuild::mission_control::parse_platforms(&platforms)?;
            let config = orbitbuild::mission_control::MissionControlConfig {
                beacon,
                platforms,
                socket_dir: std::path::PathBuf::from("/tmp"),
                data_dir,
                builder_name: "orbit".to_string(),
            };
            orbitbuild::mission_control::run_mission_control(config).await?;
        }
        Commands::Status {
            wait,
            timeout_secs,
            platforms,
        } => {
            tracing::info!(wait, timeout_secs, platforms = %platforms, "status");
            let platforms = orbitbuild::mission_control::parse_platforms(&platforms)?;
            let config = orbitbuild::status::StatusConfig {
                platforms,
                socket_dir: std::path::PathBuf::from("/tmp"),
                wait,
                timeout_secs,
            };
            orbitbuild::status::run_status(config).await?;
        }
        Commands::Link { beacon, platforms } => {
            tracing::info!(beacon = %beacon, platforms = %platforms, "link");
            let beacon: Beacon = beacon.parse()?;
            let platforms = orbitbuild::mission_control::parse_platforms(&platforms)?;
            let config = orbitbuild::link::LinkConfig {
                beacon,
                platforms,
                data_dir,
            };
            orbitbuild::link::run_link(config).await?;
        }
        Commands::Fleet { beacon } => {
            tracing::info!(beacon = %beacon, "fleet");
            let beacon: Beacon = beacon.parse()?;
            let config = orbitbuild::fleet::FleetConfig {
                beacon,
                data_dir,
            };
            orbitbuild::fleet::run_fleet(config).await?;
        }
    }

    Ok(())
}
