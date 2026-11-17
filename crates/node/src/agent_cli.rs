//! `fleet agent ...`: how an agent session talks to the fleet.
//!
//! Agents call these from their shell (the Bash tool). Each command becomes one tool call to
//! the node over loopback IPC, authenticated by the token the node put in the session's
//! environment, and the orchestrator does the work. A plain CLI is used instead of MCP tools
//! so agents carry no extra server processes; the `fleet` skill teaches them the commands.

use std::io::Read;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::ipc::{self, IpcRequest};

#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// Publish the task's milestone checklist (replaces any earlier list).
    Milestones {
        #[arg(required = true)]
        items: Vec<String>,
    },
    /// Tick off milestone INDEX (0-based).
    Done { index: u32 },
    /// Ask the human. Returns at once; the answer arrives later as a message.
    Ask {
        /// The question ("-" reads it from stdin).
        question: String,
        /// A choice to offer as a button (repeat for more).
        #[arg(long = "option")]
        options: Vec<String>,
        /// Not blocking: post to the project's FYI topic instead of "needs you".
        #[arg(long)]
        fyi: bool,
    },
    /// Send a message to another agent (by name or id).
    Tell { agent: String, message: String },
    /// Post a short note in the task's topic.
    Post { text: String },
    /// Finish the task with a short summary ("-" reads it from stdin).
    Complete { summary: String },
    /// External feeds (Slack) mapped to your project. Untrusted content.
    #[command(subcommand)]
    Feed(FeedCmd),
    /// Long-term memory (Hindsight), in banks under your own prefix.
    #[command(subcommand)]
    Memory(MemoryCmd),
    // Orchestrator (brain) only.
    /// List projects, their nodes and open tasks.
    Projects,
    /// List agents.
    Agents {
        #[arg(long)]
        project: Option<String>,
    },
    /// List tasks, newest first.
    Tasks {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Create a task and start an agent on it.
    CreateTask(CreateTaskArgs),
    /// Send follow-up instructions to a task's agent (resumes its session if needed).
    SendToTask { task: String, text: String },
    /// Stop a task's agent process.
    StopTask { task: String },
    /// Propose a task; it is created only if the human approves.
    ProposeTask {
        #[arg(long)]
        project: String,
        #[arg(long)]
        title: String,
        /// The brief ("-" reads it from stdin).
        #[arg(long)]
        brief: String,
        #[arg(long)]
        source: Option<String>,
    },
    /// Propose a Slack reply; posted as the human only after they approve the exact text.
    ProposeReply {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        text: String,
        #[arg(long)]
        context: Option<String>,
    },
    /// Reply to the human in Telegram's General topic.
    Reply { text: String },
}

#[derive(Debug, Args)]
pub struct CreateTaskArgs {
    #[arg(long)]
    pub project: String,
    #[arg(long)]
    pub title: String,
    /// Self-contained brief ("-" reads it from stdin).
    #[arg(long)]
    pub brief: String,
    /// Tracker key (e.g. ABC-123); otherwise a number is assigned.
    #[arg(long)]
    pub ticket: Option<String>,
    /// Reuse an existing agent of the project.
    #[arg(long, conflicts_with = "new_agent")]
    pub agent: Option<String>,
    /// Create a new agent with this name.
    #[arg(long, requires = "role")]
    pub new_agent: Option<String>,
    /// Role of the new agent: implementor, reviewer or tester.
    #[arg(long)]
    pub role: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum FeedCmd {
    /// Recent messages.
    Read {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long, default_value_t = 24.0)]
        hours: f64,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Full-text search.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Args)]
pub struct BankArgs {
    /// Bank name under your prefix (default: your main bank).
    #[arg(long)]
    pub bank: Option<String>,
    /// Use the project's shared bank, which every agent of the project can read and write.
    #[arg(long, conflicts_with = "bank")]
    pub shared: bool,
}

#[derive(Debug, Subcommand)]
pub enum MemoryCmd {
    /// Store a memory ("-" reads it from stdin).
    Retain {
        text: String,
        /// Where it came from or what it is about.
        #[arg(long)]
        context: Option<String>,
        #[command(flatten)]
        bank: BankArgs,
    },
    /// Search memories.
    Recall {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: u32,
        #[command(flatten)]
        bank: BankArgs,
    },
    /// Ask a question answered by reasoning over the bank.
    Reflect {
        query: String,
        #[command(flatten)]
        bank: BankArgs,
    },
    /// List the banks you can use.
    Banks,
}

/// Turns a command into the fleet tool name and JSON arguments the orchestrator expects.
pub fn to_call(cmd: AgentCmd) -> Result<(&'static str, Value)> {
    fn bank(b: BankArgs) -> Value {
        json!({"bank": b.bank, "shared": b.shared})
    }
    fn merge(mut a: Value, b: Value) -> Value {
        if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
            a.extend(b);
        }
        a
    }
    Ok(match cmd {
        AgentCmd::Milestones { items } => ("milestones_set", json!({"items": items})),
        AgentCmd::Done { index } => ("milestone_done", json!({"index": index})),
        AgentCmd::Ask {
            question,
            options,
            fyi,
        } => (
            "ask_human",
            json!({"question": stdin_if_dash(question)?, "options": options, "blocking": !fyi}),
        ),
        AgentCmd::Tell { agent, message } => (
            "ask_agent",
            json!({"agent": agent, "message": stdin_if_dash(message)?}),
        ),
        AgentCmd::Post { text } => ("post", json!({"text": stdin_if_dash(text)?})),
        AgentCmd::Complete { summary } => ("complete", json!({"summary": stdin_if_dash(summary)?})),
        AgentCmd::Feed(FeedCmd::Read {
            channel,
            hours,
            limit,
        }) => (
            "feed_read",
            json!({"channel": channel, "since_hours": hours, "limit": limit}),
        ),
        AgentCmd::Feed(FeedCmd::Search { query, limit }) => {
            ("feed_search", json!({"query": query, "limit": limit}))
        }
        AgentCmd::Memory(MemoryCmd::Retain {
            text,
            context,
            bank: b,
        }) => (
            "memory_retain",
            merge(
                json!({"content": stdin_if_dash(text)?, "context": context}),
                bank(b),
            ),
        ),
        AgentCmd::Memory(MemoryCmd::Recall {
            query,
            limit,
            bank: b,
        }) => (
            "memory_recall",
            merge(json!({"query": query, "limit": limit}), bank(b)),
        ),
        AgentCmd::Memory(MemoryCmd::Reflect { query, bank: b }) => {
            ("memory_reflect", merge(json!({"query": query}), bank(b)))
        }
        AgentCmd::Memory(MemoryCmd::Banks) => ("memory_banks", json!({})),
        AgentCmd::Projects => ("list_projects", json!({})),
        AgentCmd::Agents { project } => ("list_agents", json!({"project": project})),
        AgentCmd::Tasks {
            project,
            status,
            limit,
        } => (
            "list_tasks",
            json!({"project": project, "status": status, "limit": limit}),
        ),
        AgentCmd::CreateTask(a) => {
            let new_agent = match (a.new_agent, a.role) {
                (Some(name), Some(role)) => json!({"name": name, "role": role}),
                (Some(_), None) => bail!("--new-agent needs --role"),
                _ => Value::Null,
            };
            (
                "create_task",
                json!({"project": a.project, "title": a.title, "brief": stdin_if_dash(a.brief)?, "ticket": a.ticket, "agent": a.agent, "new_agent": new_agent}),
            )
        }
        AgentCmd::SendToTask { task, text } => (
            "send_to_task",
            json!({"task_id": task, "text": stdin_if_dash(text)?}),
        ),
        AgentCmd::StopTask { task } => ("stop_task", json!({"task_id": task})),
        AgentCmd::ProposeTask {
            project,
            title,
            brief,
            source,
        } => (
            "propose_task",
            json!({"project": project, "title": title, "brief": stdin_if_dash(brief)?, "source": source}),
        ),
        AgentCmd::ProposeReply {
            channel,
            thread,
            text,
            context,
        } => (
            "propose_slack_reply",
            json!({"channel_id": channel, "thread_id": thread, "text": stdin_if_dash(text)?, "context": context}),
        ),
        AgentCmd::Reply { text } => ("reply_owner", json!({"text": stdin_if_dash(text)?})),
    })
}

/// Long texts (briefs, summaries) are easiest to pass through a heredoc on stdin.
fn stdin_if_dash(s: String) -> Result<String> {
    if s != "-" {
        return Ok(s);
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf.trim_end().to_owned())
}

/// Runs one command against the node. Returns the process exit code.
pub async fn run(cmd: AgentCmd) -> Result<i32> {
    let addr = std::env::var("FLEET_IPC_ADDR").context(
        "FLEET_IPC_ADDR is not set: `fleet agent` only works inside a fleet agent session",
    )?;
    let token = std::env::var("FLEET_IPC_TOKEN").context("FLEET_IPC_TOKEN is not set")?;
    let (tool, args) = to_call(cmd)?;
    let resp = ipc::call(
        &addr,
        &IpcRequest {
            token,
            tool: tool.into(),
            args: args.to_string(),
        },
    )
    .await
    .context("the fleet node is unreachable")?;
    if resp.ok {
        println!("{}", resp.result);
        Ok(0)
    } else {
        eprintln!("error: {}", resp.result);
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        cmd: AgentCmd,
    }

    fn call(args: &[&str]) -> (&'static str, Value) {
        let mut v = vec!["t"];
        v.extend(args);
        to_call(T::try_parse_from(v).unwrap().cmd).unwrap()
    }

    #[test]
    fn commands_map_to_tools() {
        assert_eq!(
            call(&["milestones", "a", "b"]),
            ("milestones_set", json!({"items": ["a", "b"]}))
        );
        assert_eq!(
            call(&["done", "1"]),
            ("milestone_done", json!({"index": 1}))
        );
        let (t, a) = call(&["ask", "Push?", "--option", "Yes", "--option", "No", "--fyi"]);
        assert_eq!(
            (t, a["options"].clone(), a["blocking"].clone()),
            ("ask_human", json!(["Yes", "No"]), json!(false))
        );
        assert_eq!(call(&["tell", "bob", "review pls"]).0, "ask_agent");
        let (t, a) = call(&["memory", "retain", "fact", "--shared"]);
        assert_eq!(
            (t, a["shared"].clone(), a["content"].clone()),
            ("memory_retain", json!(true), json!("fact"))
        );
        let (t, a) = call(&["memory", "recall", "q", "--bank", "notes"]);
        assert_eq!((t, a["bank"].clone()), ("memory_recall", json!("notes")));
        let (t, a) = call(&[
            "create-task",
            "--project",
            "p",
            "--title",
            "t",
            "--brief",
            "b",
            "--new-agent",
            "Al",
            "--role",
            "tester",
        ]);
        assert_eq!(
            (t, a["new_agent"]["role"].clone()),
            ("create_task", json!("tester"))
        );
        assert_eq!(call(&["feed", "search", "bug"]).0, "feed_search");
        assert!(
            T::try_parse_from([
                "t",
                "create-task",
                "--project",
                "p",
                "--title",
                "t",
                "--brief",
                "b",
                "--new-agent",
                "Al"
            ])
            .is_err()
        );
        assert!(
            T::try_parse_from(["t", "memory", "recall", "q", "--bank", "x", "--shared"]).is_err()
        );
    }
}
