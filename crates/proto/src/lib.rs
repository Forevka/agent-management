//! Wire protocol between the orchestrator and its nodes.
//!
//! Every frame on the control stream is one [`Envelope`]. Envelopes carry a per-direction
//! sequence number so the link layer can deliver each message exactly once across reconnects:
//! the receiver acknowledges the highest sequence number it has processed, and the sender
//! replays everything newer after a reconnect. Sequence number `0` marks control frames
//! (handshake, acks, heartbeats) that are not part of the reliable stream.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`Body`] or [`Envelope`]. Peers with different
/// versions refuse each other during the handshake rather than misreading frames.
///
/// 2: [`Body::SendToRun`]; the orchestrator no longer sends [`Body::SendToAgent`], which an
/// older node could not tell apart from a message for its current run.
/// 3: [`Body::ProjectList`]. A version 2 node cannot decode it, and an undecodable frame drops
/// the link, so it must be refused at the handshake instead.
///
/// The messages added in release v0.2.4 needed no bump: the orchestrator sends
/// [`Body::UpdateAvailable`] and [`Body::DiskReportWanted`] only to nodes that can read them
/// ([`reads_v0_2_4_messages`]), and a node sends [`Body::DiskReport`] only to an orchestrator
/// that asked for it on the current connection — so no node already out there is refused, and an
/// orchestrator rolled back to an older release is never sent a frame it cannot decode. Prefer
/// this to a bump. From v0.2.4 on, a refusal for a protocol mismatch also carries an
/// [`update_hint`], so a node can update itself across later bumps instead of waiting for a person.
pub const PROTOCOL_VERSION: u32 = 3;

/// The build this binary is: the release that produced it (`v0.2.7`), or the workspace version
/// with `-dev` for a binary built by hand.
///
/// This, not `CARGO_PKG_VERSION`, is what a node reports and what the fleet compares against the
/// newest release to decide that an update is due. The workspace version never changes between
/// releases, so a fleet that compared it would see every machine as up to date for ever. The
/// release workflow sets `FLEET_VERSION` from the tag; rustc records the variable as a build
/// input, so a new value recompiles this crate rather than reusing a stale one.
pub const BUILD_VERSION: &str = match option_env!("FLEET_VERSION") {
    // Set but empty is not a version: a Docker `ARG` declared with no value arrives that way,
    // and a binary reporting "" would look up to date with nothing and behind everything.
    Some(v) if !v.is_empty() => v,
    _ => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

/// ALPN for the control protocol. The version is part of it so a future protocol can be
/// served next to this one during a rolling upgrade.
pub const ALPN: &[u8] = b"agent-fleet/control/1";

/// Upper bound on one encoded frame. Large payloads (transcripts, session archives) are
/// chunked by the sender instead of growing this.
pub const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

pub mod agent;
pub mod node;
pub mod redact;

pub use agent::{
    AgentEvent, AgentEventKind, SecretFile, SpawnAgent, StopAgent, ToolCall, ToolResult,
};
pub use node::{
    ClaudeAuth, DiskReport, DiskVolume, FolderSize, Hello, NodeInfo, NodeSettings, ProjectMapping,
    ProjectSummary, Rejected, Welcome,
};

/// One frame on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Sequence number in the sender's reliable stream, starting at 1; `0` for control frames.
    pub seq: u64,
    /// Highest sequence number of the peer's stream that the sender has processed.
    pub ack: u64,
    pub body: Body,
}

impl Envelope {
    pub fn control(ack: u64, body: Body) -> Self {
        Self { seq: 0, ack, body }
    }

    pub fn is_control(&self) -> bool {
        self.seq == 0
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let bytes = postcard::to_stdvec(self)?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(bytes.len()));
        }
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// Everything that can be said on the control stream.
///
/// New variants are only ever appended: postcard encodes the variant index, so reordering
/// would silently change the meaning of frames from older peers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Body {
    // Handshake and link control (always sent with seq 0).
    Hello(Hello),
    Welcome(Welcome),
    Rejected(Rejected),
    Heartbeat,
    /// Carries only the `ack` field; sent when there is nothing else to piggyback it on.
    Ack,

    // Node -> orchestrator.
    NodeInfo(NodeInfo),
    AgentEvent(AgentEvent),

    // Orchestrator -> node.
    SpawnAgent(Box<SpawnAgent>),
    StopAgent(StopAgent),
    /// Free-form text for an agent's running session (a reply, or a message from another agent).
    SendToAgent {
        agent_id: String,
        task_id: String,
        text: String,
    },

    // Appended in protocol version 1 before its first release.
    /// Node -> orchestrator: an agent called one of the fleet's MCP tools.
    ToolCall(ToolCall),
    /// Orchestrator -> node: the answer to a [`ToolCall`], matched by `call_id`.
    ToolResult(ToolResult),
    /// Orchestrator -> node: how this node starts Claude Code. Sent on every connect.
    NodeSettings(NodeSettings),

    // Appended in protocol version 2.
    /// Orchestrator -> node: text for one specific run of an agent. The node delivers it only if
    /// that run is the agent's current one; otherwise it hands the text back as
    /// [`AgentEventKind::Undelivered`], tagged with this `run_id`, so the orchestrator
    /// can tell a run that is gone from one it has already replaced.
    SendToRun {
        agent_id: String,
        task_id: String,
        run_id: String,
        text: String,
    },

    // Appended in protocol version 3.
    /// Orchestrator -> node: every project, so the node's tray app can offer them for mapping.
    /// Sent on every connect (after [`Body::NodeSettings`]) and after any project change.
    ProjectList(Vec<ProjectSummary>),

    // Appended in protocol version 3, after release v0.2.3. Never sent to a node that does not
    // [`reads_v0_2_4_messages`]: an older node cannot decode it, and an undecodable frame drops
    // its link.
    /// Orchestrator -> node: a newer release is running here; update to it. Sent on connect to a
    /// node whose version is behind. See [`UpdateAvailable`].
    UpdateAvailable(UpdateAvailable),

    // Appended in protocol version 3, after release v0.2.3. Sent only to an orchestrator that
    // asked for it ([`Body::DiskReportWanted`]) on the current connection.
    /// Node -> orchestrator: free space where Claude Code and the projects write. Sent when
    /// asked and every few minutes after, so the orchestrator can warn the owner and stop
    /// starting runs on a node whose disk is about to fill up.
    DiskReport(DiskReport),

    // Appended in protocol version 3, after release v0.2.3. Never sent to a node that does not
    // [`reads_v0_2_4_messages`].
    /// Orchestrator -> node, on connect: this orchestrator reads [`Body::DiskReport`]; send them.
    DiskReportWanted,
}

/// Which release a node should move to, and where to fetch it.
///
/// The node downloads `<base_url>/fleet-<version without v>-<target>`, checks it against
/// `<base_url>/SHA256SUMS`, checks the binary itself says it is `version`, and swaps it in once no
/// run is active. Both values come from the orchestrator over the authenticated link, and
/// `release.yml` only publishes tags that are on `main`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateAvailable {
    /// The release tag, e.g. `v0.2.7`.
    pub version: String,
    /// Where that release's assets are, e.g.
    /// `https://github.com/<owner>/<repo>/releases/download/v0.2.7`.
    pub base_url: String,
}

/// The line a refusal for a protocol mismatch ends with, telling the node what to update to.
///
/// A node on an older protocol is refused at the handshake, before any [`Body::UpdateAvailable`]
/// could reach it, and the only thing it is guaranteed to decode is [`Rejected`]'s free-text
/// `reason`. So the instruction travels inside that text, in a form that must never change:
/// every node from release v0.2.4 on looks for exactly this. (It is also why [`Hello`] must never
/// change: the orchestrator has to be able to read an old node's hello to refuse it this way.)
pub const UPDATE_HINT_PREFIX: &str = "update: ";

/// `update: v0.2.7 https://…/releases/download/v0.2.7`, appended to a refusal's reason.
pub fn update_hint(u: &UpdateAvailable) -> String {
    format!("{UPDATE_HINT_PREFIX}{} {}", u.version, u.base_url)
}

/// The GitHub repository (`owner/repo`) this build's releases are published from. Fixed at build
/// time (`FLEET_RELEASE_REPO`), so a node only ever downloads its replacement from the place its
/// own binary came from — not from wherever an orchestrator, or anyone who took one over, names.
pub const RELEASE_REPO: &str = match option_env!("FLEET_RELEASE_REPO") {
    Some(r) if !r.is_empty() => r,
    _ => "Forevka/agent-management",
};

/// Where release `version`'s assets are: the only `base_url` a node accepts.
pub fn release_base_url(version: &str) -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/{version}")
}

/// `v0.2.7` -> (0, 2, 7). Anything else (a hand build's `0.1.0-dev`, a pre-release, garbage) is
/// not a release.
pub fn release_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.strip_prefix('v')?.split('.');
    let n = |p: Option<&str>| p?.parse::<u64>().ok();
    let version = (n(parts.next())?, n(parts.next())?, n(parts.next())?);
    parts.next().is_none().then_some(version)
}

/// The newest release that cannot decode the messages added in v0.2.4.
const LAST_RELEASE_WITHOUT_V0_2_4: (u64, u64, u64) = (0, 2, 3);

/// Whether a node reporting `app_version` can decode the messages appended after release v0.2.3
/// ([`Body::UpdateAvailable`], [`Body::DiskReportWanted`]): a release newer than v0.2.3, or the
/// very build this binary is (a test, or a hand build talking to itself). Any other hand build
/// is sent none — it may be older than the variants.
pub fn reads_v0_2_4_messages(app_version: &str) -> bool {
    app_version == BUILD_VERSION
        || release_version(app_version).is_some_and(|v| v > LAST_RELEASE_WITHOUT_V0_2_4)
}

/// Whether a node on `app_version` reads every message this build sends: it is this very build,
/// or a release at least as new as this one.
///
/// **The gate for every orchestrator -> node message added after v0.2.4.** A node behind the
/// orchestrator is offered the orchestrator's release first ([`Body::UpdateAvailable`]) and
/// updates itself within minutes; once it reconnects level, it gets the new messages too. So a
/// feature never needs to know which release it will ship in, and no node is ever sent a variant
/// it cannot decode (an undecodable frame drops the link). A node newer than its orchestrator
/// (a rollback) reads everything it is sent. The other direction needs no gate of its own: a node
/// sends a new message only in answer to one of these, which only an orchestrator that reads the
/// answer sends.
pub fn node_is_current(app_version: &str) -> bool {
    if app_version == BUILD_VERSION {
        return true;
    }
    match (release_version(app_version), release_version(BUILD_VERSION)) {
        (Some(node), Some(this)) => node >= this,
        _ => false,
    }
}

/// Finds an [`update_hint`] line in a refusal's reason.
pub fn parse_update_hint(reason: &str) -> Option<UpdateAvailable> {
    reason.lines().find_map(|l| {
        let (version, base_url) = l.trim().strip_prefix(UPDATE_HINT_PREFIX)?.split_once(' ')?;
        (version.starts_with('v') && base_url.starts_with("https://")).then(|| UpdateAvailable {
            version: version.to_owned(),
            base_url: base_url.trim().to_owned(),
        })
    })
}

impl Body {
    /// Control bodies are never queued for reliable delivery.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Body::Hello(_) | Body::Welcome(_) | Body::Rejected(_) | Body::Heartbeat | Body::Ack
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
    FrameTooLarge(usize),
    #[error("malformed frame: {0}")]
    Codec(#[from] postcard::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(env: Envelope) {
        let bytes = env.encode().unwrap();
        assert_eq!(Envelope::decode(&bytes).unwrap(), env);
    }

    #[test]
    fn an_update_instruction_survives_the_trip_through_a_refusal() {
        let u = UpdateAvailable {
            version: "v0.3.1".into(),
            base_url: "https://github.com/o/r/releases/download/v0.3.1".into(),
        };
        roundtrip(Envelope::control(0, Body::UpdateAvailable(u.clone())));
        // As the server writes it: a human sentence, then the hint on its own line.
        let reason = format!(
            "protocol version 4 is not supported (orchestrator speaks 5); update the node\n{}",
            update_hint(&u)
        );
        assert_eq!(parse_update_hint(&reason), Some(u));
        // Nothing to act on in an ordinary refusal, or in a hint that is not a release over https:
        // a node must never fetch a binary from a plain-http or made-up address.
        for reason in [
            "unknown node: use an invite",
            "update: v0.3.1",
            "update: v0.3.1 http://example.com/x",
            "update: 0.3.1 https://example.com/x",
            "please update: v0.3.1 https://example.com/x but not like this",
        ] {
            assert_eq!(parse_update_hint(reason), None, "{reason:?}");
        }
    }

    #[test]
    fn new_messages_wait_until_a_node_is_level_with_this_build() {
        // The same build always reads its own messages.
        assert!(node_is_current(BUILD_VERSION));
        match release_version(BUILD_VERSION) {
            Some((a, b, c)) => {
                assert!(node_is_current(&format!("v{a}.{b}.{c}")));
                assert!(
                    node_is_current(&format!("v{a}.{b}.{}", c + 1)),
                    "newer reads all"
                );
                if c > 0 {
                    assert!(
                        !node_is_current(&format!("v{a}.{b}.{}", c - 1)),
                        "behind: not yet"
                    );
                }
            }
            // A hand build: only itself, never a release or another hand build.
            None => {
                assert!(!node_is_current("v0.2.5"));
                assert!(!node_is_current("0.0.1-dev"));
            }
        }
        assert!(!node_is_current(""));
    }

    #[test]
    fn only_a_release_tag_is_a_release() {
        assert_eq!(release_version("v0.2.7"), Some((0, 2, 7)));
        assert_eq!(release_version("v1.10.0"), Some((1, 10, 0)));
        for not in [
            "0.2.7",
            "0.1.0-dev",
            "v0.3.0-rc.1",
            "v0.2",
            "v0.2.7.1",
            "vx.y.z",
            "",
        ] {
            assert_eq!(release_version(not), None, "{not:?}");
        }
    }

    #[test]
    fn new_messages_go_only_to_nodes_that_can_read_them() {
        // Every release up to v0.2.3 would drop its link on the unknown variants.
        for old in ["v0.2.0", "v0.2.3", "v0.1.9"] {
            assert!(!reads_v0_2_4_messages(old), "{old}");
        }
        for new in ["v0.2.4", "v0.2.10", "v0.3.0", "v1.0.0"] {
            assert!(reads_v0_2_4_messages(new), "{new}");
        }
        // Other hand builds, and binaries from before releases existed: never.
        for other in ["0.1.0", "0.0.9-dev", "v0.3.0-rc.1", ""] {
            assert!(!reads_v0_2_4_messages(other), "{other:?}");
        }
        // The same build always reads its own messages.
        assert!(reads_v0_2_4_messages(BUILD_VERSION));
    }

    #[test]
    fn the_build_version_is_a_release_or_says_it_is_not_one() {
        // An empty FLEET_VERSION (how CI passes "not a release") must not make an empty version:
        // a node would report nothing, and the orchestrator could not tell it from a release.
        let v = std::hint::black_box(BUILD_VERSION);
        assert!(v.starts_with('v') || v.ends_with("-dev"), "{v:?}");
    }

    #[test]
    fn every_body_roundtrips() {
        roundtrip(Envelope::control(
            3,
            Body::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                node_name: "pc".into(),
                app_version: "0.1.0".into(),
                os: "windows".into(),
                join_secret: Some("s".into()),
                last_received_seq: 7,
                incarnation: 9,
                last_received_from: 8,
            }),
        ));
        roundtrip(Envelope::control(
            0,
            Body::Welcome(Welcome {
                last_received_seq: 2,
                incarnation: 3,
            }),
        ));
        roundtrip(Envelope::control(
            0,
            Body::Rejected(Rejected {
                reason: "no".into(),
            }),
        ));
        roundtrip(Envelope::control(9, Body::Heartbeat));
        roundtrip(Envelope::control(9, Body::Ack));
        roundtrip(Envelope {
            seq: 1,
            ack: 0,
            body: Body::NodeInfo(NodeInfo {
                projects: vec![ProjectMapping {
                    project: "stoneshard".into(),
                    path: "D:/dev/x".into(),
                }],
                max_agents: 4,
                running_agents: vec!["a1".into()],
                running_runs: vec!["r1".into()],
            }),
        });
        roundtrip(Envelope {
            seq: 2,
            ack: 1,
            body: Body::SpawnAgent(Box::new(SpawnAgent {
                agent_id: "a1".into(),
                task_id: "stoneshard-12".into(),
                project: "stoneshard".into(),
                role: "implementor".into(),
                prompt: "do X".into(),
                append_system_prompt: Some("you are an implementor".into()),
                resume_session_id: None,
                permission_mode: "plan".into(),
                after_plan_mode: Some("auto".into()),
                model: Some("opus".into()),
                extra_args: vec![],
                extra_mcp_servers: Some("{}".into()),
                run_id: "r1".into(),
                secret_files: vec![SecretFile {
                    name: "jira".into(),
                    content: "JIRA_TOKEN=x\n".into(),
                }],
            })),
        });
        roundtrip(Envelope {
            seq: 3,
            ack: 2,
            body: Body::AgentEvent(AgentEvent {
                agent_id: "a1".into(),
                task_id: "stoneshard-12".into(),
                kind: AgentEventKind::Output {
                    json: "{\"type\":\"assistant\"}".into(),
                },
                run_id: "r1".into(),
            }),
        });
        roundtrip(Envelope {
            seq: 4,
            ack: 2,
            body: Body::StopAgent(StopAgent {
                agent_id: "a1".into(),
                task_id: "t".into(),
                graceful: true,
                run_id: "r1".into(),
            }),
        });
        roundtrip(Envelope {
            seq: 6,
            ack: 2,
            body: Body::SendToRun {
                agent_id: "a1".into(),
                task_id: "t".into(),
                run_id: "r1".into(),
                text: "hi".into(),
            },
        });
        roundtrip(Envelope {
            seq: 5,
            ack: 2,
            body: Body::SendToAgent {
                agent_id: "a1".into(),
                task_id: "t".into(),
                text: "hi".into(),
            },
        });
    }

    #[test]
    fn rpc_and_settings_roundtrip() {
        roundtrip(Envelope {
            seq: 6,
            ack: 0,
            body: Body::ToolCall(ToolCall {
                call_id: "c1".into(),
                agent_id: "a1".into(),
                task_id: "t".into(),
                tool: "ask_human".into(),
                args: "{}".into(),
            }),
        });
        roundtrip(Envelope {
            seq: 7,
            ack: 0,
            body: Body::ToolResult(ToolResult {
                call_id: "c1".into(),
                ok: true,
                result: "done".into(),
            }),
        });
        roundtrip(Envelope {
            seq: 8,
            ack: 0,
            body: Body::NodeSettings(NodeSettings {
                claude_command: vec!["claude".into()],
                auth: ClaudeAuth::OAuthToken("t".into()),
                max_agents: 4,
            }),
        });
        roundtrip(Envelope {
            seq: 9,
            ack: 0,
            body: Body::ProjectList(vec![
                ProjectSummary {
                    slug: "demo".into(),
                    name: "Demo".into(),
                },
                ProjectSummary {
                    slug: "fleet".into(),
                    name: String::new(),
                },
            ]),
        });
        roundtrip(Envelope {
            seq: 10,
            ack: 0,
            body: Body::ProjectList(vec![]),
        });
        roundtrip(Envelope {
            seq: 11,
            ack: 0,
            body: Body::DiskReportWanted,
        });
        roundtrip(Envelope {
            seq: 12,
            ack: 0,
            body: Body::DiskReport(DiskReport {
                claude_dir: "C:/Users/me/.claude".into(),
                volumes: vec![DiskVolume {
                    volume: "C:\\".into(),
                    free: 3 << 30,
                    total: 512 << 30,
                    claude_dir: true,
                    projects: vec!["fleet-src".into()],
                }],
                biggest: vec![FolderSize {
                    name: "C--repos-fleet-src".into(),
                    bytes: 7 << 30,
                }],
            }),
        });
    }

    #[test]
    fn oversized_frames_are_refused() {
        let env = Envelope {
            seq: 1,
            ack: 0,
            body: Body::SendToAgent {
                agent_id: "a".into(),
                task_id: "t".into(),
                text: "x".repeat(MAX_FRAME_LEN + 1),
            },
        };
        assert!(matches!(env.encode(), Err(ProtoError::FrameTooLarge(_))));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(Envelope::decode(&[0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
    }
}
