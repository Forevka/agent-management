//! Endpoint construction shared by the orchestrator and nodes.

use std::net::SocketAddr;

use anyhow::Result;
use iroh::{Endpoint, RelayMode, RelayUrl, SecretKey, endpoint::presets};

/// Which relay servers an endpoint uses to get through NAT when a direct path fails.
#[derive(Debug, Clone)]
pub enum RelayChoice {
    /// No relays: direct IP connections only (tests, or a LAN-only fleet).
    Disabled,
    /// The fleet's own relay on the VPS. This is the production setting, so the fleet does
    /// not depend on third-party infrastructure.
    Custom(RelayUrl),
    /// number 0's public relays. Handy before the self-hosted relay is up.
    N0Public,
}

/// Binds an endpoint with a fixed identity.
///
/// No address-lookup service is configured: nodes dial the orchestrator with the full address
/// from their invite ticket (relay URL plus IPs), so nothing is published to public DNS.
pub async fn bind_endpoint(
    secret: SecretKey,
    relay: RelayChoice,
    bind_addr: Option<SocketAddr>,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint> {
    let relay_mode = match relay {
        RelayChoice::Disabled => RelayMode::Disabled,
        RelayChoice::Custom(url) => RelayMode::custom([url]),
        RelayChoice::N0Public => RelayMode::Default,
    };
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .relay_mode(relay_mode)
        .alpns(alpns);
    if let Some(addr) = bind_addr {
        builder = builder.bind_addr(addr)?;
    }
    Ok(builder.bind().await?)
}
