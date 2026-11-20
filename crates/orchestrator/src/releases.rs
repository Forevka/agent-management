//! Which release this orchestrator is, and which nodes should move to it.
//!
//! A merge to `main` becomes a release, and the VPS updates its orchestrator to it within minutes
//! (deploy/update). The orchestrator then brings its nodes along: on connect, a node whose version
//! is behind is told which release to fetch ([`fleet_proto::UpdateAvailable`]); and a node too old
//! to speak this protocol is told the same inside its refusal (see `fleet_net::Server`). Nodes do
//! the rest themselves (`fleet_node::update`), and only where their supervisor allows it.
//!
//! Only a real release offers anything: a hand-built orchestrator (`0.1.0-dev`) has no published
//! binaries for its nodes to fetch, and must never pull them onto one.

use fleet_proto::{BUILD_VERSION, UpdateAvailable, release_base_url, release_version};

/// This orchestrator as an update target, or `None` when it is not a release.
pub fn this_release() -> Option<UpdateAvailable> {
    release_version(BUILD_VERSION)?;
    Some(UpdateAvailable {
        version: BUILD_VERSION.to_owned(),
        base_url: release_base_url(BUILD_VERSION),
    })
}

/// Whether a node reporting `node` should be told to move to `this`.
///
/// Only a node that can read the notice ([`fleet_proto::understands_update_notices`]: a release
/// after v0.2.3) and is on an *older* release. A node on a newer release than this orchestrator is
/// left alone — it was installed from a release the VPS has not deployed yet, and will be level
/// within minutes. Never a downgrade, and never a hand build.
pub fn should_offer(this: &str, node: &str) -> bool {
    match (release_version(this), release_version(node)) {
        (Some(this), Some(n)) => fleet_proto::understands_update_notices(node) && n < this,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_brought_up_to_the_orchestrator_and_never_down() {
        // Behind, and able to read the notice: offered.
        assert!(should_offer("v0.2.7", "v0.2.6"));
        // Numbers, not text: v0.2.10 is newer than v0.2.9.
        assert!(should_offer("v0.2.10", "v0.2.9"));
        assert!(!should_offer("v0.2.9", "v0.2.10"), "never a downgrade");
        // Level: nothing to do.
        assert!(!should_offer("v0.2.7", "v0.2.7"));
        // Behind but too old to decode the notice: it would drop the link, so nothing is sent.
        assert!(!should_offer("v0.2.7", "v0.2.3"));
        // A hand build, a pre-release, a binary from before releases: left alone.
        for other in ["0.1.0", "0.1.0-dev", "v0.3.0-rc.1"] {
            assert!(!should_offer("v0.2.7", other), "{other}");
        }
        // A hand-built orchestrator offers nothing to anyone: it has no binaries to give.
        assert!(!should_offer("0.1.0-dev", "v0.2.4"));
    }

    #[test]
    fn a_release_points_at_its_own_assets() {
        // Only meaningful in a release build; a hand build offers nothing.
        match this_release() {
            Some(u) => {
                assert_eq!(u.version, BUILD_VERSION);
                assert!(
                    u.base_url
                        .ends_with(&format!("/releases/download/{BUILD_VERSION}"))
                );
            }
            None => assert!(BUILD_VERSION.ends_with("-dev")),
        }
    }
}
