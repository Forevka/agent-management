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
pub const PROTOCOL_VERSION: u32 = 2;

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
pub use node::{ClaudeAuth, Hello, NodeInfo, NodeSettings, ProjectMapping, Rejected, Welcome};

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
