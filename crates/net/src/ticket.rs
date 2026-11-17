//! Invite tickets: everything a new node needs to join, in one copy-pasteable string.

use std::{fmt, str::FromStr};

use anyhow::{Context, Result, bail};
use data_encoding::BASE64URL_NOPAD;
use iroh::EndpointAddr;
use rand::RngCore;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "fleetinvite1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteTicket {
    /// Where to dial: the orchestrator's key plus its relay URL and/or IP addresses.
    pub orchestrator: EndpointAddr,
    /// Single-use secret the orchestrator checks in the node's first hello.
    pub secret: String,
}

impl InviteTicket {
    pub fn new(orchestrator: EndpointAddr) -> Self {
        let mut bytes = [0u8; 24];
        rand::rng().fill_bytes(&mut bytes);
        Self {
            orchestrator,
            secret: BASE64URL_NOPAD.encode(&bytes),
        }
    }
}

impl fmt::Display for InviteTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = postcard::to_stdvec(self).map_err(|_| fmt::Error)?;
        write!(f, "{PREFIX}{}", BASE64URL_NOPAD.encode(&bytes))
    }
}

impl FromStr for InviteTicket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let Some(body) = s.trim().strip_prefix(PREFIX) else {
            bail!("not a fleet invite ticket (expected it to start with {PREFIX})");
        };
        let bytes = BASE64URL_NOPAD
            .decode(body.as_bytes())
            .context("invite ticket is not valid base64")?;
        postcard::from_bytes(&bytes).context("invite ticket is corrupted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn ticket_roundtrips_through_text() {
        let addr = EndpointAddr::new(SecretKey::generate().public())
            .with_ip_addr("203.0.113.5:7700".parse().unwrap());
        let t = InviteTicket::new(addr);
        let parsed: InviteTicket = t.to_string().parse().unwrap();
        assert_eq!(parsed, t);
        assert!("garbage".parse::<InviteTicket>().is_err());
        assert!(format!("{PREFIX}!!!").parse::<InviteTicket>().is_err());
    }
}
