//! Shared Router setup — spawns iroh Router with Blobs, Gossip, and Docs protocols.
//!
//! Every OrbitBuild node (Station, Satellite, Mission Control) needs the same
//! foundation: an iroh `Router` with blobs, gossip, and docs ALPN handlers.
//! This module provides a builder that sets up all three and allows additional
//! custom protocols (e.g., the BuildProxy on Satellites).

use anyhow::{Context, Result};
use iroh::{
    endpoint::presets,
    protocol::Router,
    Endpoint, SecretKey,
};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::protocol::Docs;
use iroh_gossip::net::Gossip;

use crate::keys;

/// How long to wait for the endpoint to come online (connect to relay).
const ONLINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Result of spawning the shared infrastructure.
pub struct NodeSetup {
    /// The iroh Router (owns the accept loop, manages graceful shutdown).
    pub router: Router,
    /// The Docs protocol — used to create, import, and query documents.
    pub docs: Docs,
    /// The iroh Endpoint.
    pub endpoint: Endpoint,
    /// The Blobs store — used to read doc entry values by content hash.
    pub blobs: iroh_blobs::store::mem::MemStore,
}

/// Builder for NodeSetup. Allows adding extra protocols to the Router.
pub struct NodeBuilder {
    secret_key: SecretKey,
    extra_protocols: Vec<(Vec<u8>, Box<dyn iroh::protocol::DynProtocolHandler>)>,
}

impl NodeBuilder {
    /// Create a new NodeBuilder with the given secret key.
    pub fn new(secret_key: SecretKey) -> Self {
        Self {
            secret_key,
            extra_protocols: Vec::new(),
        }
    }

    /// Create a NodeBuilder that loads or generates a key from the given data directory.
    pub fn from_data_dir(data_dir: &std::path::Path) -> Result<Self> {
        let key_path = keys::key_path(data_dir);
        let secret_key = keys::load_or_generate_secret_key(&key_path)?;
        Ok(Self::new(secret_key))
    }

    /// Register an additional protocol on the Router.
    pub fn accept(
        mut self,
        alpn: impl AsRef<[u8]>,
        handler: impl Into<Box<dyn iroh::protocol::DynProtocolHandler>>,
    ) -> Self {
        self.extra_protocols
            .push((alpn.as_ref().to_vec(), handler.into()));
        self
    }

    /// Build the endpoint, spawn protocols, and return the NodeSetup.
    pub async fn spawn(self) -> Result<NodeSetup> {
        // Collect all ALPNs we need to register on the endpoint
        let mut alpns: Vec<Vec<u8>> = vec![
            iroh_blobs::protocol::ALPN.to_vec(),
            iroh_gossip::net::GOSSIP_ALPN.to_vec(),
            iroh_docs::net::ALPN.to_vec(),
        ];
        for (alpn, _) in &self.extra_protocols {
            alpns.push(alpn.clone());
        }

        // Create iroh endpoint
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(self.secret_key)
            .alpns(alpns)
            .bind()
            .await
            .context("failed to bind iroh endpoint")?;

        // Wait for endpoint to come online (discover relay, direct addresses)
        let online_result = tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online()).await;
        if online_result.is_err() {
            tracing::warn!("timed out waiting for endpoint to come online (relay may be slow)");
        }

        let addr = endpoint.addr();
        tracing::info!(node_id = %addr.id, "endpoint ready");

        // Create Blobs store (in-memory) — MemStore derefs to api::Store
        let blob_store = MemStore::new();
        let blobs_protocol = iroh_blobs::BlobsProtocol::new(&blob_store, None);
        let blobs_store = blob_store.clone(); // MemStore clone gives us an api::Store

        // Create Gossip protocol
        let gossip = Gossip::builder().spawn(endpoint.clone());

        // Create Docs protocol
        let docs = Docs::memory()
            .spawn(endpoint.clone(), blobs_store.into(), gossip.clone())
            .await
            .context("failed to spawn docs protocol")?;

        // Build Router with all protocols
        let mut router_builder = Router::builder(endpoint.clone())
            .accept(iroh_blobs::protocol::ALPN, blobs_protocol)
            .accept(iroh_gossip::net::GOSSIP_ALPN, gossip)
            .accept(iroh_docs::net::ALPN, docs.clone());

        // Add extra protocols (e.g., BuildProxy on Satellites)
        for (alpn, handler) in self.extra_protocols {
            router_builder = router_builder.accept(alpn, handler);
        }

        let router = router_builder.spawn();

        Ok(NodeSetup {
            router,
            docs,
            endpoint,
            blobs: blob_store,
        })
    }
}
