# Agent Fleet: plan

## Context

The goal is to manage a fleet of Claude Code agents from one Telegram group.

- One host (the VPS) is the **orchestrator**.
- Every other host (Windows PCs, and the VPS too) runs a **node**.
- Nodes and the orchestrator talk over **iroh**, an encrypted two-way peer-to-peer link.

You send requests in Telegram. The orchestrator:

- works out which project a request belongs to;
- hands it to a suitable agent, or creates a new one. Each agent has its own Telegram bot identity.

That agent then:

- writes a plan;
- gets the plan approved through a web page;
- does the work and reports progress as a checklist in its own topic.

Two kinds of things also flow into Telegram:

- Slack messages are mirrored into project feed topics, where agents can see them.
- Anything that needs your input goes to per-project "needs you" and "FYI" topics.

The orchestrator remembers projects and tasks over time through **Hindsight**, a self-hosted memory server.

This is a new project, unrelated to `stoneshard-mod`. It goes in **`Forevka/agent-management`**, cloned into this session.

## Decisions (from the interview)

| Area | Decision |
|---|---|
| Scale | Large: 10+ hosts, dozens of agents. **Postgres** (with pgvector, which Hindsight also uses). |
| Hosts | Windows PCs run a Windows service; the Linux VPS runs the orchestrator and a node. |
| Users | Only you: a Telegram user-ID allowlist. Every other user is ignored. |
| Claude per node | Configured per node: (1) the **start command** (path and extra args), (2) **auth**: either the login Claude already has (subscription) or a **token** you set (`CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY`). Scaling up means adding nodes and accounts. |
| Permissions | `--permission-mode auto`. The brief tells the agent to create its own git worktree. When it needs you, the question goes to the human queue. |
| Isolation | A git worktree only. No Docker. |
| Model | Opus everywhere (can be changed per role or per task). |
| Request flow | "do X" → orchestrator finds the project → sends it to the agent that did similar work, or creates a new agent from the bot pool → that agent plans in **plan mode** → plan page on the VPS (answer its questions, approve) → work → later questions come as Telegram buttons or free-text replies. |
| Agents | **Persistent specialists**, each pinned to one project. Their Claude `session_id`s are stored so follow-up work (e.g. client feedback) resumes the same session. |
| Bot pool | You make bots with BotFather and register their tokens. The orchestrator assigns them, renames them (`setMyName`/`setMyDescription`) and warns you when few are left. |
| Telegram output | A plan link, then a **milestone checklist** that gets ticked off, then a short summary. No tool calls and no reasoning text. |
| Topics | One per task, named `{proj}-{ticket}`. Per project there are also `{proj} · needs you` and `{proj} · fyi`, plus a feed topic for each mapped Slack channel. |
| Ticket IDs | The tracker key if there is one (e.g. Jira `ABC-123`), otherwise an auto number per project. |
| Agent-to-agent | Only through the orchestrator. No native Telegram bot-to-bot messages. |
| Roles at launch | Implementor, Reviewer, Tester. |
| Projects | Registered by hand. A project is created once on the server (its configs and secrets), and each node maps a local folder to it. |
| Git, PRs, rules | Each project's `AGENTS.md`/`CLAUDE.md` sets the rules. When the agent is unsure, it asks you through the human queue. |
| Secrets | A free-form secrets manager on the server: folders of key/value pairs, with presets such as Jira. At spawn time the node writes a `.env` file **outside the project**. The brief says how to read a single key (`fleet secret get` / your skills' scripts) so the whole file never lands in the transcript. |
| Feeds | Slack first, behind an adapter interface so other sources can be added later. Each Slack channel maps to a project feed topic. **Only your own Slack user token is available: no Slack app, no bot, no Socket Mode.** So the adapter **polls** the Web API (`conversations.history` / `conversations.replies`) with that token. It polls at an adaptive interval and backs off on HTTP 429. It can only see channels you are in. Agents only read Slack. The orchestrator can **propose tasks** and **propose replies**. An approved reply goes out through `chat.postMessage` with your token, so it **appears as you**. Approval is therefore always required, and the proposal shows the exact text. |
| Unanswered questions | Reminders only. Agents never answer on your behalf. |
| Memory | Hindsight on the VPS, used as an MCP server by the orchestrator brain and by agents (one memory bank per project and per agent). Its internal LLM is **Haiku via an Anthropic API key**. |
| Orchestrator brain | A long-lived Claude Code session on the VPS with the fleet and Hindsight MCP servers. |
| Web UI (v1) | Plan pages (magic links), project setup and secrets, nodes, agents and the bot pool. |
| Deploy | Docker Compose on the VPS: orchestrator, Postgres+pgvector, Hindsight, iroh-relay, Caddy (HTTPS on your subdomain). |
| Updates | Signed releases. They are pushed to nodes after you approve in Telegram. Busy nodes update once their agents finish. |
| Limits | No limits on turns, time or cost. |
| Retention | Transcripts, feed messages and audit logs are kept 90 days. Memories and task records are kept forever. Nightly Postgres backups. |

## Architecture

```
                         ┌───────────────────────── VPS (docker compose) ─────────────────────────┐
 You ── Telegram group ──┤ fleet orchestrator                                                    │
 (topics, buttons)       │  ├─ tg: orchestrator bot (admin, long-poll) + agent bots (send, cb)  │
 Slack ◄─ poll (user tk)─┤  ├─ feeds: Source trait → SlackSource (more later)                    │
 Browser ── HTTPS ───────┤  ├─ web: axum + htmx (plan pages, projects/secrets, nodes/agents)     │
                         │  ├─ router/bus, scheduler, human queue, secrets vault (age-encrypted) │
                         │  ├─ brain: Claude Code session (fleet-orch MCP + Hindsight MCP)      │
                         │  └─ net: iroh endpoint (ALPN fleet/1)                                 │
                         │ postgres+pgvector · hindsight · iroh-relay · caddy · fleet node (VPS) │
                         └──────────────────────────────▲─────────────────────────────────────────┘
                                                        │ iroh QUIC (key-pinned, NAT traversal)
                     ┌──────────────────────────────────┴──────────────┐
               fleet node (Windows service)                      fleet node (…)
                ├─ agent supervisor → claude -p (stream-json) per agent session
                ├─ local IPC (named pipe / unix socket) ← `fleet mcp` stdio shim (fleet tools for Claude)
                ├─ secrets drop: <data>/secrets/<task>/*.env (user-only ACL, wiped at task end)
                └─ updater, project mappings, session archive upload
```

### Rust workspace (new repo)
- `crates/proto`: versioned wire types (serde + postcard). Every message is wrapped in an envelope `{id, ack_of, ts, kind}`.
- `crates/net`: the iroh endpoint, ALPN `fleet/1`, length-prefixed frames, reconnect with backoff, and a durable outbox that resends until the other side confirms receipt.
- `crates/orchestrator`: state (sqlx + Postgres, migrations), scheduler, message router, human queue, Telegram, feeds, web, vault, brain supervisor.
- `crates/node`: the Claude runner, agent supervisor, IPC server, secrets drop, updater, Windows service (`windows-service`) / systemd.
- `crates/mcp`: the `fleet mcp --agent <id>` stdio MCP shim (`rmcp`), which talks to the local node over IPC.
- `crates/cli` → a single binary `fleet` with subcommands: `orchestrator | node | mcp | invite | join | secret | project …`.
- `deploy/`: `docker-compose.yml`, Caddyfile, iroh-relay config, Hindsight config.

### Data model (Postgres)
| Group | Tables |
|---|---|
| Hosts | `nodes` (iroh key, name, OS, online, version), `node_claude_config` (start command, auth mode, encrypted token ref) |
| Projects and secrets | `projects` (slug, name, tracker preset), `project_locations` (node, path), `secret_folders`/`secret_items` (age-encrypted values), `project_secret_bindings` |
| Agents | `bots` (token encrypted, assigned agent), `agents` (name, role, project, bot, memory bank, preferred node) |
| Work | `tasks` (proj, ticket, title, status, topic_id, agent, node, branch), `sessions` (claude session_id, node, cwd, state, archived JSONL ref), `milestones` |
| Messages and humans | `messages` (the message bus), `human_items` (kind, question, options, state, tg message ref, reminders), `magic_links` |
| Feeds and audit | `feed_sources`, `feed_channels` (→ project, topic), `feed_messages` (FTS), `audit_log` (secret reads, approvals, spawns) |

## Key flows

1. **Joining a node:**
   - Run `fleet invite` on the VPS. It produces a one-time ticket that contains the orchestrator's iroh address and an expiring secret.
   - Run `fleet join <ticket>` on the PC. Its key is added to the allowlist, and it installs and starts itself as a service.
   - The node's Claude command and auth are then set in the web UI.
2. **Mapping a project:** create the project once in the web UI (tracker preset, secret bindings, Slack channels). On a node, run `fleet project map <slug> <path>` or use the web UI.
3. **"do X" in Telegram (General, or a project topic):**
   - The orchestrator bot receives it and passes it to the **brain** session. The brain:
     - looks up memory in Hindsight;
     - lists projects and agents;
     - decides the project and ticket (it can create a tracker issue through your skills or MCP if the project's rules say so);
     - picks an existing specialist on that project (by similarity of past work) or creates a new agent with a free bot;
     - calls `create_task(...)` with a brief.
   - The orchestrator then creates the topic `{proj}-{ticket}` and sends `SpawnAgent` to the node that has the project (preferring the agent's usual host). If that node is offline, the task waits in a queue and you get a note in `needs you`.
4. **Agent run on the node:**
   - The node writes the secrets `.env` files.
   - It runs the configured start command with `-p --input-format stream-json --output-format stream-json --verbose --model opus --permission-mode plan --mcp-config <fleet+hindsight> --permission-prompt-tool mcp__fleet__permission --append-system-prompt <role + brief + worktree/secrets/milestone instructions>`, or adds `--resume <session_id>` when resuming.
   - It streams events back to the orchestrator, which saves the transcript.
5. **Plan approval:**
   - The agent calls `ExitPlanMode`. It goes to the `fleet.permission` tool, which turns it into a **plan page**, along with any `AskUserQuestion` calls made during planning.
   - You get a magic link in the task topic and in `needs you`.
   - On the page you answer the questions and approve, or send feedback.
   - The tool returns allow (with the answers in `updatedInput`), and the session switches to `auto` mode.
6. **Execution:**
   - The agent calls `fleet.milestones_set([...])` once and `fleet.milestone_done(i)` as it goes, which edits the checklist message.
   - `fleet.ask_human(question, options?, blocking)` creates an item in `{proj} · needs you` (or `fyi`) with buttons, and accepts a free-text reply to that message. The answer goes back into the session.
   - `fleet.ask_agent` / `fleet.post` send messages to other agents through the message bus (stored, confirmed on receipt, with a hop limit to stop loops), and a short copy appears in the topic.
   - At the end, `fleet.complete(summary)` posts a summary, closes the topic and stores memories in Hindsight.
7. **Follow-up on old work:** the brain recognises the task or agent. The topic is reopened, and the stored session is resumed with `--resume` on the same node and folder. Session JSONL files are archived on the VPS so a session can be restored if a node is rebuilt.
8. **Slack (user token, polling):**
   - The adapter keeps a cursor per channel (the last `ts` seen) and polls each mapped channel's history. It also checks threads that are still active (`conversations.replies`) for a while. Busy channels are polled more often and quiet ones less. Every request goes through one rate-limit budget per token.
   - The adapter stores each message (as untrusted data) and mirrors it into the project's feed topic. If the channel is busy, messages are batched into digests to stay within Telegram limits.
   - Agents read Slack through `feed_read` / `feed_search`.
   - The brain may post a **task proposal** or a **reply proposal** in `needs you`. A reply is sent to Slack only after you approve it.
9. **Reminders:** open `needs you` items are re-pinged after 2 hours, never auto-answered. `/queue` lists open items across projects.

### Fleet MCP tools for agents
`milestones_set`, `milestone_done`, `ask_human`, `ask_agent`, `post`, `inbox`, `feed_read`, `feed_search`, `secret_get` (audited; returns a single key), `complete`, `permission` (internal).

The brain additionally gets `list_projects`, `list_agents`, `create_agent`, `create_task`, `resume_task`, `propose_task`, `propose_slack_reply`.

### Telegram details
- **Orchestrator bot:**
  - is a group admin with `can_manage_topics`;
  - has privacy mode off, so it reads everything;
  - creates and closes topics and routes your messages.
- **Agent bots** only send messages, and are polled only for presses on their buttons.
- Each bot has its own send queue with a rate limiter (about 20 messages a minute per group).
- Checklists:
  - The plan is native checklists (`sendChecklist`). These may only be available to business accounts, which the phase 0 spike checks.
  - The fallback is a single message with ☐/☑ lines that gets edited.

## Phases

| # | Scope | Done when |
|---|---|---|
| 0. Spikes | (a) Headless plan mode: route `ExitPlanMode` and `AskUserQuestion` through `--permission-prompt-tool`, then switch to `auto` after approval. (b) `--permission-mode auto` in `-p` mode with both auth modes. (c) Can a bot use checklists in a group? (d) Self-hosted iroh-relay, with a PC behind NAT running as a Windows service. (e) `--resume` after a process is killed, and where session files are stored. (f) Hindsight MCP with per-project and per-agent banks. (g) Slack with your user token: which token type it is (`xoxp` from an existing workspace app, or a browser session token), the real rate limits on `conversations.history`, and whether polling covers your channels without being throttled. | Each one shown working in a small throwaway program; findings written up |
| 1. Skeleton | Workspace; proto and net (outbox, acks); invite/join; compose stack; Postgres migrations; node service; per-node Claude config; start one agent from the CLI and stream its transcript | `fleet task run` on the VPS starts Claude on your PC and the transcript appears on the VPS |
| 2. Telegram and human queue | Orchestrator bot plus the bot pool, topics, checklist, summary, fleet MCP shim, `ask_human` with buttons and replies, reminders | A task started by hand runs from start to finish through Telegram, including a question you answer |
| 3. Brain, planning, memory | Brain session, Hindsight, `create_task` routing to existing agents, plan pages with magic links, session resume for follow-ups | "do X" in General ends with an approved plan and finished work, and a follow-up reuses the same session |
| 4. Web UI and secrets | Project setup, secrets manager (folders, presets), secrets drop and `secret get`, nodes, agents, bot-pool screens, audit log | A Jira-backed project is set up from the UI and an agent reads one secret without it appearing in its transcript |
| 5. Slack feeds | `Source` trait, Slack adapter that polls with your user token, channel mapping, digests, `feed_*` tools, task and reply proposals | A Slack bug report leads to a proposal you approve, and a reply goes back to Slack after you approve it |
| 6. Hardening | Restart recovery (orchestrator, node, Claude process), signed releases and updates on approval, retention and backups, metrics and logs (`tracing`), failure-injection tests, Windows and Linux release builds in CI | Kill the VPS, the PC or the network in the middle of a task: it resumes with no lost messages |

## Verification

- **Automated tests (CI):**
  - a fake `claude` binary that emits scripted stream-json, including ExitPlanMode and AskUserQuestion permission calls;
  - a mock Telegram Bot API server;
  - a mock Slack.
- **Integration tests:** the orchestrator and two nodes run in one process on loopback iroh, with Postgres via testcontainers. Each flow from "Key flows" is an end-to-end scenario test.
- **Failure-injection tests:** randomly drop iroh connections and kill node or Claude processes, then check that every message arrives exactly once and every session resumes.
- **Manual acceptance on real hosts:** the VPS plus your Windows PC with both auth modes. Run the phase exit scenarios above in a real Telegram group with a real Slack workspace.
- **Security checks:**
  - an unknown Telegram user is ignored;
  - an unknown iroh key is refused;
  - an expired magic link is rejected;
  - no secret value appears in transcripts or logs (grep the archived transcripts in CI scenarios).

## Things to prepare
- An empty GitHub repo added to the session.
- The VPS subdomain DNS records (web + relay).
- A Telegram supergroup with topics enabled, the orchestrator bot (admin) and a first batch of agent bots.
- Your Slack user token, stored in the secrets manager, and the list of channels to map.
- An Anthropic API key for Hindsight.

Sources consulted: [Telegram bot-to-bot](https://core.telegram.org/api/bots/bot-to-bot), [Hindsight](https://hindsight.vectorize.io/blog/2026/03/04/mcp-agent-memory), [Hindsight + Claude Code](https://www.starryhope.com/ai/hindsight-memory-for-coding-agents/).
