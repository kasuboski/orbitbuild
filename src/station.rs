//! Station — the Constellation seed node.
//!
//! The Station is the persistent seed that bootstraps the Constellation. It:
//! - Creates the shared iroh-docs Document (the CRDT "phonebook")
//! - Shares the Doc via a Beacon (DocTicket)
//! - Stays online so other peers can bootstrap via the Beacon
//!
//! The Station does NOT maintain a phonebook itself — the Doc IS the phonebook.
//! Satellite registration and discovery are handled by the Doc CRDT automatically.

use anyhow::{Context, Result};

use crate::beacon::Beacon;
use crate::router::NodeBuilder;

/// Run `station init` — create a new Constellation.
///
/// Creates the iroh Endpoint, spawns the Router with Blobs+Gossip+Docs,
/// creates a new Document, shares it as a Beacon, and waits for ctrl-c.
pub async fn run_station_init(data_dir: &std::path::Path) -> Result<()> {
    // Build node infrastructure (endpoint + router + blobs + gossip + docs)
    let setup = NodeBuilder::from_data_dir(data_dir)?
        .spawn()
        .await
        .context("failed to spawn station node")?;

    let endpoint = &setup.endpoint;
    let docs = &setup.docs;

    // Create a new Doc
    let doc = docs.create().await.context("failed to create document")?;

    // Create a default author for writing to the Doc
    let _author = docs
        .author_create()
        .await
        .context("failed to create author")?;

    tracing::info!(doc_id = %doc.id(), "created constellation document");

    // Share the Doc with write capability and include our relay address
    let addr = endpoint.addr();
    let doc_ticket = doc
        .share(
            iroh_docs::api::protocol::ShareMode::Write,
            iroh_docs::api::protocol::AddrInfoOptions::Relay,
        )
        .await
        .context("failed to share document")?;

    // Wrap DocTicket as Beacon
    let beacon = Beacon::new(doc_ticket);

    // Print Beacon to stdout for the user to copy
    println!("ORBIT_BEACON={beacon}");
    eprintln!();
    eprintln!("Constellation initialized! Share the beacon with Satellites and Mission Control:");
    eprintln!("  ORBIT_BEACON={beacon}");
    eprintln!();
    eprintln!("Station node ID: {:?}", addr.id);
    eprintln!("Document ID: {}", doc.id());
    eprintln!();
    eprintln!("Station is listening... (Ctrl+C to stop)");

    // Wait for ctrl-c, then graceful shutdown
    tokio::signal::ctrl_c().await?;
    eprintln!("\nShutting down station...");
    setup.router.shutdown().await?;
    eprintln!("Station stopped.");

    Ok(())
}

/// Run `station join` — join an existing Constellation as a backup seed.
///
/// Imports the Doc from the Beacon's DocTicket and acts as an additional
/// sync peer / seed node.
pub async fn run_station_join(beacon: &Beacon, data_dir: &std::path::Path) -> Result<()> {
    let setup = NodeBuilder::from_data_dir(data_dir)?
        .spawn()
        .await
        .context("failed to spawn station node")?;

    let docs = &setup.docs;

    // Import the Doc from the Beacon's ticket
    let doc = docs
        .import(beacon.doc_ticket().clone())
        .await
        .context("failed to import document from beacon")?;

    // Create an author for any local writes
    let _author = docs
        .author_create()
        .await
        .context("failed to create author")?;

    tracing::info!(doc_id = %doc.id(), "joined constellation document");

    eprintln!("Backup station joined the Constellation.");
    eprintln!("Document ID: {}", doc.id());
    eprintln!();
    eprintln!("Backup station is listening... (Ctrl+C to stop)");

    // Wait for ctrl-c, then graceful shutdown
    tokio::signal::ctrl_c().await?;
    eprintln!("\nShutting down backup station...");
    setup.router.shutdown().await?;
    eprintln!("Backup station stopped.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (sans-IO: pure logic only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Station tests are integration-level (require networking).
    // Sans-IO logic is tested via Beacon, keys, and Doc entry serialization tests.
}
