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
    /// Mark milestone INDEX (0-based) as skipped: the work turned out not to need it.
    Skip {
        index: u32,
        /// Why it was not needed (shown next to it).
        #[arg(long)]
        reason: Option<String>,
    },
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
    /// Post a short note in the task's topic. Silent unless --notify.
    Post {
        text: String,
        /// Notify the owner with a sound: only for something they should see now.
        #[arg(long)]
        notify: bool,
    },
    /// Finish the task with a short summary ("-" reads it from stdin).
    Complete { summary: String },
    /// Like complete, but the topic stays open and can be triggered again: on a schedule, or
    /// whenever the owner writes into it. Use this instead of `complete` to turn the task into
    /// a workflow.
    WorkflowFinish {
        /// Short summary of what this workflow does ("-" reads it from stdin).
        summary: String,
        /// How often it runs on its own: every:15m, every:30m, every:1h, every:6h or
        /// daily:HH:MM. Omit for on-demand only (triggered by writing into the topic).
        #[arg(long)]
        schedule: Option<String>,
    },
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
    /// Pass the owner's wishes on to the secretary (e.g. what to ignore, what is a todo).
    SecretaryNote { text: String },
    // Secretary only.
    /// The owner's todo list.
    #[command(subcommand)]
    Todo(TodoCmd),
    /// Hand messages to the orchestrator: work for an agent, or a reply worth drafting.
    Escalate {
        /// What it is about and why it needs the orchestrator ("-" reads it from stdin).
        #[arg(long)]
        summary: String,
        /// A message ref from the batch (repeat for more).
        #[arg(long = "source", required = true)]
        sources: Vec<String>,
    },
    /// Post to the owner's Inbox topic.
    #[command(subcommand)]
    Digest(DigestCmd),
    /// Draft a reply to a message, sent as the owner only once they tap Send. Only when the owner
    /// asks for one, in answer to their own message.
    ReplyDraft {
        /// The message to answer (its ref).
        #[arg(long = "ref")]
        reference: String,
        /// The reply ("-" reads it from stdin).
        #[arg(long)]
        text: String,
    },
    /// The owner's triage rules.
    #[command(subcommand)]
    Rule(RuleCmd),
    /// Record messages you ignore or note (todos and escalations are recorded by their commands).
    Triage {
        /// ignore or note.
        #[arg(value_parser = ["ignore", "note"])]
        decision: String,
        /// Message refs.
        #[arg(required = true)]
        refs: Vec<String>,
        /// Why, in a few words.
        #[arg(long)]
        why: Option<String>,
    },
    /// Administer the fleet: reads run at once, every change waits for the owner's approval.
    Admin(AdminArgs),
}

#[derive(Debug, Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub cmd: AdminCmd,
    /// Why you propose the change; shown to the owner as your note ("-" reads it from stdin).
    #[arg(long, global = true)]
    pub note: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AdminCmd {
    /// Read the fleet's setup: projects, project <slug>, agents [project], bots, nodes,
    /// node <id|name>, secrets, secret <folder>, status, items. Never shows secret values.
    Show {
        what: String,
        target: Option<String>,
    },
    /// Propose a new project.
    ProjectCreate {
        slug: String,
        #[arg(long)]
        name: Option<String>,
        /// What the project is ("-" reads it from stdin).
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Propose a new name or description for a project.
    ProjectEdit {
        slug: String,
        #[arg(long)]
        name: Option<String>,
        /// "-" reads it from stdin.
        #[arg(long)]
        description: Option<String>,
    },
    /// Propose giving a project's agents a secret folder (by name or id).
    ProjectBind { project: String, folder: String },
    /// Propose taking a secret folder away from a project.
    ProjectUnbind { project: String, folder: String },
    /// Propose mirroring a Slack channel (its id, e.g. C0123ABCD) into a project.
    ChannelAdd {
        project: String,
        channel: String,
        /// Show new messages to the orchestrator, which may propose tasks or replies.
        #[arg(long)]
        triage: bool,
    },
    /// Propose removing a Slack channel from a project.
    ChannelRemove { project: String, channel: String },
    /// Propose renaming an agent (its bot's Telegram name follows).
    AgentRename { agent: String, name: String },
    /// Propose giving an agent's bot back to the pool.
    AgentUnbind { agent: String },
    /// Propose removing an agent that has no active tasks.
    AgentRemove { agent: String },
    /// Propose an agent's default Claude model and effort ("" goes back to the default).
    AgentSet {
        agent: String,
        #[arg(long)]
        model: Option<String>,
        /// low, medium, high, xhigh or max.
        #[arg(long)]
        effort: Option<String>,
    },
    /// Propose removing a free bot from the pool.
    BotRemove { bot: String },
    /// Propose new settings for a node. Tokens and API keys are set on the web only.
    /// The rest of the Claude command (program, other arguments) is set on the web only.
    NodeSet {
        node: String,
        /// Set Claude's --model ("" removes it).
        #[arg(long)]
        model: Option<String>,
        /// Set Claude's --fallback-model ("" removes it).
        #[arg(long)]
        fallback_model: Option<String>,
        /// default (Claude's own login), oauth_token or api_key.
        #[arg(long)]
        auth: Option<String>,
        #[arg(long)]
        max_agents: Option<i32>,
    },
    /// Propose revoking a node (it must be invited again to rejoin).
    NodeRevoke { node: String },
    /// Propose an empty secret folder; the owner sets its values on the web.
    FolderCreate {
        name: String,
        /// custom, jira, github, gitlab, slack or ssh.
        #[arg(long)]
        preset: Option<String>,
    },
    /// Propose deleting a secret folder and its values.
    FolderDelete { folder: String },
    /// Propose deleting one key's value from a secret folder.
    SecretUnset { folder: String, key: String },
    /// Send the owner a login link to a web page, e.g. to enter a token or secret value there
    /// (/agents, /secrets/<id>, /nodes/<id>, /projects/<slug>). You do not get the link.
    Link {
        page: String,
        /// One line telling the owner what to do there.
        #[arg(long)]
        text: Option<String>,
    },
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
    /// The task's own Claude model (else the agent's default); within the owner's allow-list.
    #[arg(long)]
    pub model: Option<String>,
    /// The task's own effort: low, medium, high, xhigh or max (else the agent's default).
    #[arg(long)]
    pub effort: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum TodoCmd {
    /// Add a todo for the owner.
    Add {
        /// Short and actionable: who, what, by when.
        #[arg(long)]
        title: String,
        /// Context ("-" reads it from stdin).
        #[arg(long)]
        detail: Option<String>,
        /// A message ref it came from (repeat for more).
        #[arg(long = "source")]
        sources: Vec<String>,
        /// YYYY-MM-DD.
        #[arg(long)]
        due: Option<String>,
    },
    /// List todos (open by default).
    List {
        /// open, done, dismissed or all.
        #[arg(long, default_value = "open")]
        status: String,
    },
    /// Change a todo's status.
    Update {
        /// T-12 or 12.
        id: String,
        /// open, done or dismissed.
        #[arg(long, value_parser = ["open", "done", "dismissed"])]
        status: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DigestCmd {
    /// Post a summary or an answer to the owner.
    Post {
        /// The text ("-" reads it from stdin).
        #[arg(long)]
        text: String,
        /// A message ref it is about (repeat for more): shown with where it came from, and the
        /// owner can reply to the post about it.
        #[arg(long = "source")]
        sources: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum RuleCmd {
    /// Add a rule the owner asked for.
    Add {
        #[arg(long)]
        text: String,
        /// The owner's words that asked for it.
        #[arg(long)]
        because: String,
    },
    /// List the active rules.
    List,
    /// Remove a rule (R-3 or 3).
    Remove {
        id: String,
        /// The owner's words that asked for it.
        #[arg(long)]
        because: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum FeedCmd {
    /// Recent messages, or the messages given with --ref.
    Read {
        /// A conversation's name or id.
        #[arg(long, visible_alias = "conversation")]
        channel: Option<String>,
        /// A message ref (repeat for more); reads exactly those messages.
        #[arg(long = "ref")]
        refs: Vec<String>,
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
        AgentCmd::Skip { index, reason } => {
            ("milestone_skip", json!({"index": index, "reason": reason}))
        }
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
        AgentCmd::Post { text, notify } => (
            "post",
            json!({"text": stdin_if_dash(text)?, "notify": notify}),
        ),
        AgentCmd::Complete { summary } => ("complete", json!({"summary": stdin_if_dash(summary)?})),
        AgentCmd::WorkflowFinish { summary, schedule } => (
            "workflow_finish",
            json!({"summary": stdin_if_dash(summary)?, "schedule": schedule}),
        ),
        AgentCmd::Feed(FeedCmd::Read {
            channel,
            refs,
            hours,
            limit,
        }) => (
            "feed_read",
            json!({"channel": channel, "refs": refs, "since_hours": hours, "limit": limit}),
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
                json!({"project": a.project, "title": a.title, "brief": stdin_if_dash(a.brief)?, "ticket": a.ticket, "agent": a.agent, "new_agent": new_agent, "model": a.model, "effort": a.effort}),
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
        AgentCmd::SecretaryNote { text } => {
            ("secretary_note", json!({"text": stdin_if_dash(text)?}))
        }
        AgentCmd::Todo(TodoCmd::Add {
            title,
            detail,
            sources,
            due,
        }) => (
            "todo_add",
            json!({"title": title, "detail": detail.map(stdin_if_dash).transpose()?, "refs": sources, "due": due}),
        ),
        AgentCmd::Todo(TodoCmd::List { status }) => ("todo_list", json!({"status": status})),
        AgentCmd::Todo(TodoCmd::Update { id, status }) => {
            ("todo_update", json!({"id": id, "status": status}))
        }
        AgentCmd::Escalate { summary, sources } => (
            "escalate",
            json!({"summary": stdin_if_dash(summary)?, "refs": sources}),
        ),
        AgentCmd::Digest(DigestCmd::Post { text, sources }) => (
            "digest_post",
            json!({"text": stdin_if_dash(text)?, "refs": sources}),
        ),
        AgentCmd::ReplyDraft { reference, text } => (
            "reply_draft",
            json!({"ref": reference, "text": stdin_if_dash(text)?}),
        ),
        AgentCmd::Rule(RuleCmd::Add { text, because }) => {
            ("rule_add", json!({"text": text, "because": because}))
        }
        AgentCmd::Rule(RuleCmd::List) => ("rule_list", json!({})),
        AgentCmd::Rule(RuleCmd::Remove { id, because }) => {
            ("rule_remove", json!({"id": id, "because": because}))
        }
        AgentCmd::Triage {
            decision,
            refs,
            why,
        } => (
            "triage",
            json!({"decision": decision, "refs": refs, "why": why}),
        ),
        AgentCmd::Admin(a) => admin_call(a)?,
    })
}

/// `fleet agent admin ...`: reads become `admin_read`, the link `web_link`, and every change an
/// `admin_propose` with the action name and its parameters, which the orchestrator validates.
fn admin_call(a: AdminArgs) -> Result<(&'static str, Value)> {
    // stdin can be read once: only one of the texts can come from it.
    let description_from_stdin = matches!(
        &a.cmd,
        AdminCmd::ProjectCreate { description, .. } if description == "-"
    ) || matches!(
        &a.cmd,
        AdminCmd::ProjectEdit { description: Some(d), .. } if d == "-"
    );
    if description_from_stdin && a.note.as_deref() == Some("-") {
        bail!("only one of --note and --description can be \"-\" (read from stdin)");
    }
    let propose = |action: &str, params: Value| -> Result<(&'static str, Value)> {
        let note = a.note.clone().map(stdin_if_dash).transpose()?;
        Ok((
            "admin_propose",
            json!({"action": action, "params": params, "note": note}),
        ))
    };
    match a.cmd {
        AdminCmd::Show { what, target } => {
            Ok(("admin_read", json!({"what": what, "target": target})))
        }
        AdminCmd::Link { page, text } => Ok(("web_link", json!({"page": page, "text": text}))),
        AdminCmd::ProjectCreate {
            slug,
            name,
            description,
        } => propose(
            "project_create",
            json!({"slug": slug, "name": name, "description": stdin_if_dash(description)?}),
        ),
        AdminCmd::ProjectEdit {
            slug,
            name,
            description,
        } => propose(
            "project_edit",
            json!({"slug": slug, "name": name, "description": description.map(stdin_if_dash).transpose()?}),
        ),
        AdminCmd::ProjectBind { project, folder } => propose(
            "project_bind_folder",
            json!({"project": project, "folder": folder}),
        ),
        AdminCmd::ProjectUnbind { project, folder } => propose(
            "project_unbind_folder",
            json!({"project": project, "folder": folder}),
        ),
        AdminCmd::ChannelAdd {
            project,
            channel,
            triage,
        } => propose(
            "channel_add",
            json!({"project": project, "channel": channel, "triage": triage}),
        ),
        AdminCmd::ChannelRemove { project, channel } => propose(
            "channel_remove",
            json!({"project": project, "channel": channel}),
        ),
        AdminCmd::AgentRename { agent, name } => {
            propose("agent_rename", json!({"agent": agent, "name": name}))
        }
        AdminCmd::AgentUnbind { agent } => propose("agent_unbind", json!({"agent": agent})),
        AdminCmd::AgentRemove { agent } => propose("agent_remove", json!({"agent": agent})),
        AdminCmd::AgentSet {
            agent,
            model,
            effort,
        } => propose(
            "agent_settings",
            json!({"agent": agent, "model": model, "effort": effort}),
        ),
        AdminCmd::BotRemove { bot } => propose("bot_remove", json!({"bot": bot})),
        AdminCmd::NodeSet {
            node,
            model,
            fallback_model,
            auth,
            max_agents,
        } => propose(
            "node_settings",
            json!({
                "node": node,
                "model": model,
                "fallback_model": fallback_model,
                "auth_mode": auth,
                "max_agents": max_agents,
            }),
        ),
        AdminCmd::NodeRevoke { node } => propose("node_revoke", json!({"node": node})),
        AdminCmd::FolderCreate { name, preset } => {
            propose("folder_create", json!({"name": name, "preset": preset}))
        }
        AdminCmd::FolderDelete { folder } => propose("folder_delete", json!({"folder": folder})),
        AdminCmd::SecretUnset { folder, key } => {
            propose("secret_unset", json!({"folder": folder, "key": key}))
        }
    }
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
        assert_eq!(
            call(&["skip", "0", "--reason", "already set up"]),
            (
                "milestone_skip",
                json!({"index": 0, "reason": "already set up"})
            )
        );
        assert_eq!(
            call(&["post", "Heads up", "--notify"]),
            ("post", json!({"text": "Heads up", "notify": true}))
        );
        assert_eq!(call(&["post", "fyi"]).1["notify"], json!(false));
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
        let (_, a) = call(&[
            "create-task",
            "--project",
            "p",
            "--title",
            "t",
            "--brief",
            "b",
            "--agent",
            "Al",
            "--model",
            "sonnet",
            "--effort",
            "high",
        ]);
        assert_eq!(
            (a["model"].clone(), a["effort"].clone()),
            (json!("sonnet"), json!("high"))
        );
        assert_eq!(
            call(&["admin", "agent-set", "Al", "--effort", "low"]),
            (
                "admin_propose",
                json!({"action": "agent_settings", "params": {"agent": "Al", "model": null, "effort": "low"}, "note": null})
            )
        );
        assert_eq!(call(&["feed", "search", "bug"]).0, "feed_search");
        assert_eq!(
            call(&["workflow-finish", "done", "--schedule", "every:1h"]),
            (
                "workflow_finish",
                json!({"summary": "done", "schedule": "every:1h"})
            )
        );
        assert_eq!(
            call(&["workflow-finish", "done"]),
            (
                "workflow_finish",
                json!({"summary": "done", "schedule": null})
            )
        );
        assert_eq!(
            call(&["feed", "read", "--ref", "s:c:1", "--ref", "s:c:2"]).1["refs"],
            json!(["s:c:1", "s:c:2"])
        );
        assert_eq!(
            call(&["feed", "read", "--conversation", "general"]).1["channel"],
            json!("general")
        );
        assert_eq!(
            call(&[
                "todo",
                "add",
                "--title",
                "Reply to Al",
                "--source",
                "s:c:1",
                "--due",
                "2026-11-20"
            ]),
            (
                "todo_add",
                json!({"title": "Reply to Al", "detail": null, "refs": ["s:c:1"], "due": "2026-11-20"})
            )
        );
        assert_eq!(
            call(&["todo", "update", "T-3", "--status", "done"]),
            ("todo_update", json!({"id": "T-3", "status": "done"}))
        );
        assert!(T::try_parse_from(["t", "todo", "update", "3", "--status", "gone"]).is_err());
        assert_eq!(
            call(&["escalate", "--summary", "Outage", "--source", "s:c:1"]),
            ("escalate", json!({"summary": "Outage", "refs": ["s:c:1"]}))
        );
        assert!(T::try_parse_from(["t", "escalate", "--summary", "x"]).is_err());
        assert_eq!(
            call(&[
                "rule",
                "add",
                "--text",
                "#random is noise",
                "--because",
                "ignore random"
            ]),
            (
                "rule_add",
                json!({"text": "#random is noise", "because": "ignore random"})
            )
        );
        assert_eq!(
            call(&["triage", "ignore", "s:c:1", "s:c:2", "--why", "bots"]),
            (
                "triage",
                json!({"decision": "ignore", "refs": ["s:c:1", "s:c:2"], "why": "bots"})
            )
        );
        assert!(T::try_parse_from(["t", "triage", "todo", "s:c:1"]).is_err());
        assert_eq!(
            call(&["digest", "post", "--text", "All quiet"]),
            ("digest_post", json!({"text": "All quiet", "refs": []}))
        );
        assert_eq!(
            call(&["digest", "post", "--text", "See", "--source", "s:c:1"]),
            ("digest_post", json!({"text": "See", "refs": ["s:c:1"]}))
        );
        assert_eq!(
            call(&["reply-draft", "--ref", "s:c:1", "--text", "On it"]),
            ("reply_draft", json!({"ref": "s:c:1", "text": "On it"}))
        );
        assert_eq!(
            call(&["secretary-note", "Alice is VIP"]).0,
            "secretary_note"
        );
        assert_eq!(
            call(&["admin", "show", "project", "demo"]),
            ("admin_read", json!({"what": "project", "target": "demo"}))
        );
        assert_eq!(
            call(&[
                "admin",
                "channel-add",
                "demo",
                "C01",
                "--triage",
                "--note",
                "asked"
            ]),
            (
                "admin_propose",
                json!({"action": "channel_add", "params": {"project": "demo", "channel": "C01", "triage": true}, "note": "asked"})
            )
        );
        let (t, a) = call(&[
            "admin",
            "node-set",
            "pc",
            "--model",
            "opus",
            "--max-agents",
            "2",
        ]);
        assert_eq!(
            (t, a["params"].clone()),
            (
                "admin_propose",
                json!({"node": "pc", "model": "opus", "fallback_model": null, "auth_mode": null, "max_agents": 2})
            )
        );
        // The whole command is not the brain's to set, nor --verbose (the node always passes it).
        assert!(T::try_parse_from(["t", "admin", "node-set", "pc", "--command", "sh"]).is_err());
        assert!(T::try_parse_from(["t", "admin", "node-set", "pc", "--verbose", "false"]).is_err());
        // stdin is read once.
        let both = T::try_parse_from([
            "t",
            "admin",
            "project-create",
            "web",
            "--description",
            "-",
            "--note",
            "-",
        ])
        .unwrap();
        assert!(to_call(both.cmd).is_err());
        assert_eq!(
            call(&["admin", "link", "/secrets/3"]),
            ("web_link", json!({"page": "/secrets/3", "text": null}))
        );
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
