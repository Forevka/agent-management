# Agent fleet: working notes for Claude

One Rust binary, `fleet`, runs a fleet of Claude Code agents.

- **Orchestrator.** One host (a VPS, in Docker) is the orchestrator. It keeps state in Postgres, takes the owner's commands from a Telegram forum group, serves a small web UI, and remembers things through Hindsight.
- **Nodes.** Every machine that does work runs `fleet node`: Windows PCs, and the VPS host itself. A node starts `claude -p` sessions in the owner's project directories.
- **Link.** Nodes and the orchestrator talk over iroh (QUIC, with NAT traversal through the fleet's own relay).

The design and every product decision are in `docs/plan.md`; read it before changing behaviour. Deployment is in `deploy/README.md`.

## Layout

| Path | What |
|---|---|
| `crates/proto` | Wire types: `Envelope { seq, ack, body }`, `Body` (append-only enum), handshake, spawn/tool-call messages. `PROTOCOL_VERSION`, `ALPN`. |
| `crates/net` | The control link. `Session` delivers each message exactly once, in order, across reconnects and peer restarts: outbox, acks, connection generations, process incarnations. `Server` is the orchestrator side (with an async `Authorizer`), `Client` the node side (reconnects with backoff). Also `InviteTicket`. |
| `crates/node` | Node role:<br>- `runner.rs` starts and supervises Claude, one process per *run*. It uses stream-json both ways and kills the whole process group or job object on stop. Plan approval switches the session to `auto` with a `set_permission_mode` control request. An idle session is closed and resumed on demand. Secrets are redacted from output.<br>- `agent_cli.rs` is `fleet agent ...`, how agents talk to the fleet.<br>- `skill.rs` writes the `fleet` skill plugin, loaded with `--plugin-dir`.<br>- `mcp.rs` is the `fleet mcp` stdio server. It exposes only the `permission` hook, which `--permission-prompt-tool` requires.<br>- `ipc.rs` is loopback TCP with a token per agent.<br>- `secrets.rs` writes `.env` files outside the project, per run.<br>- `config` re-exports `crates/node-config`. The node writes `status.json` (link state, the orchestrator's project list, its pid) for the tray app. |
| `crates/node-config` | `node.json` (`NodeConfig`, the mapping rules) and `status.json`, plus the atomic owner-only write every writer of those files uses. A load, change and save of `node.json` goes through `NodeConfig::update` (or holds `NodeConfig::lock`), the cross-process `node.json.lock`. `process.rs` stamps a pid with its start time. No network stack, so the tray app can depend on it. |
| `crates/orchestrator` | `memory.rs` is the only Hindsight client: one instance for everyone, with bank prefixes per agent and a shared bank per project; besides retain/recall/reflect it keeps **mental models** (a standing synthesis Hindsight refreshes itself, in `delta` mode on a cron) and bank **directives** (how facts are extracted), which is what gives the retrospective a memory across days. `core.rs` handles node events. `tasks.rs` creates tasks, places them on nodes, delivers messages and runs the brain. `human.rs` is the human queue: questions, plans, permissions, proposals. One blocking question per task is open at a time (`supersede` retires the earlier ones when the agent asks again, `close_all` when the task ends), and `owner_payload` is what an item looks like to the owner, with secrets redacted. `tools.rs` implements the fleet tools. `admin.rs` is fleet administration (projects, secrets, nodes, agents, bots), shared by the web UI and the brain, whose changes wait for the owner's approval. `jira.rs` is each project's Jira (key, optional board, one shared credential folder): the orchestrator holds the credential and does the calls, so an agent gets tools and never a token; **reads are every agent's** (its own task's project only, whatever project it names), comments and transitions are the brain's, direct and audited, create and edit are owner-approved `admin_change`s, everything is scoped to the project's key, ticket text reaches agents framed as untrusted data, and `jira check` (plus the site and account in a 401/403/404) is how a misconfigured folder says so instead of answering 404 forever. `wakeups.rs` lets an agent wait without a session: the orchestrator wakes it at a time, or when the Rhai condition the brain wrote for its plain-text wish holds (a pure predicate over a prepared snapshot — no network, git, shell, SQL or secrets, and bounded per evaluation). `habits.rs` turns what the owner keeps doing into something the fleet does for them: the brain proposes a habit in words (from a daily reflection over the owner's own decisions and requests), the owner approves it once as an ordinary `admin_change`, and from then on the orchestrator matches it **in SQL** — never an LLM at apply time — with the stoppers, the daily cap, the audit and the one-tap way back that an approval nobody tapped needs. `retro.rs` is the daily retrospective: a third standing session, held as tightly as the secretary (no secrets, `dontAsk`, only `fleet agent`), handed a digest of the fleet's own day — tasks, the owner's decisions and requests, the agents' chatter, and what went wrong — whose only writes are `retro_propose` (capped per day) and `retro_finish`; a proposal becomes a task on the fleet's own code, in one project, on the owner's tap and never otherwise. `topics.rs` owns the fleet's standing topics (Inbox for third parties reaching the owner, agent-communication for the fleet's own chatter, Rules for the secretary's rule log and the owner's habits, Retro for the retrospective's verdicts and proposals), created on first use and remembered in `kv`. `tg.rs` is the Telegram `Chat` implementation and update loops. `web.rs` serves the UI: magic links (any page, checked by `safe_target`) and the single-page app embedded from `apps/web/dist`. `api.rs` is the JSON API that app runs on — explicit response shapes, never a serialised row, and every mutation through `admin`/`human`/`secretary` as `Actor::Owner`. `auth.rs` decides node admission. `feeds.rs` covers ingestion from any source (the source-agnostic `Incoming`, opaque message refs), mirroring and the feed tools. `secretary.rs` is the secretary: batching every incoming message to its session, its tools (todos, rules, triage, escalation), the Inbox topic and its buttons, and the daily digest. `workflows.rs` is workflows: a task turned into a standing automation with `workflow_finish` keeps its topic open instead of closing it, posts a control card (schedule, Pause/Resume, Delete), and is ticked by the housekeeping loop when its schedule is due. `working.rs` shows 👀 and "typing…" while an agent works on the owner's message, and counts whether anything actually reached them: a turn that ends with the owner's message unanswered posts what the agent last said, because text an agent only writes in its session never leaves it. `vault.rs` encrypts at rest. `prompts.rs` holds role prompts. `migrations/` is the SQL schema. `e2e.rs` is the end-to-end test. |
| `crates/telegram` | Thin, typed Bot API client: rate limiting, retries, HTML and callback helpers. |
| `crates/feeds` | Source-agnostic feed model: `FeedMessage` (native ids, conversation kind, `mentions_owner`, `from_owner`), and the `Source` trait with polling, push (`run` into a `FeedSink` of `FeedEvent`s), `backfill`, and shared `Directory`/`Replier`/`Backfiller` handles. There are two Slack adapters:<br>- `SlackSocket` receives every conversation the owner is in through **Socket Mode** of the owner's own Slack app (app token `xapp-`, plus user-scoped events). It fills gaps after outages from history.<br>- `SlackSource` polls mapped channels with the **user token** alone. It is the fallback when no app token is configured.<br>Both read and post with the user token. |
| `crates/fleet` | The CLI: `orchestrator`, `invite`, `join`, `node`, `project map`, `secret get/exec`, `mcp`, `id`. On Windows, `fleet node` starts `fleet-tray.exe` from its own directory (unless `FLEET_NO_TRAY=1`). |
| `apps/web` | The web UI: React 19 + TypeScript, built with Vite. **[affectUI](https://www.affectui.com) is the golden standard for everything visual here — read `apps/web/DESIGN.md` before touching the UI.** Its free (MIT) components are vendored in `src/components/ui`, ours are in `src/components/fx` written to the same contract (one self-contained file, styles injected with React 19's `<style precedence>`, `data-*` variants, every colour from an `--aui-*` token in `src/aui.css`, dark mode through `:where(.dark, [data-theme="dark"])`, reduced motion honoured). No Tailwind, no component library, no CSS-in-JS runtime, no external fonts. Compiled into the `fleet` binary (`rust-embed`), so there is still one artefact to deploy and the UI can never be out of step with its API. Every page is a real URL, filters included (`/tasks?project=bioins&status=failed`), so the orchestrator can send the owner a link to exactly what it is talking about. Build with `npm ci && npm run build` in `apps/web`; `crates/orchestrator/build.rs` leaves a placeholder page when `dist` is missing, so a Rust-only build needs no node. |
| `apps/tray` | `fleet-tray`, the Windows tray app for mapping projects (Tauri v2 + Svelte/TypeScript). A Cargo workspace of its own, outside the root one: build it with `npm ci` and `npm run tauri build -- --no-bundle` in `apps/tray`. It depends only on `crates/node-config`. See its README. |
| `deploy/` | Docker Compose stack: orchestrator, Postgres with pgvector, Hindsight, iroh-relay, Caddy. Also the systemd unit, the Windows installer and backups. |
| `.github/workflows/` | CI (fmt, clippy, tests with Postgres, Windows build including the tray app) and release (Linux and Windows binaries, `fleet-tray.exe`, GHCR image). |

## Build and test

```sh
cd apps/web && npm ci && npm run build && cd ../..   # the UI, embedded into the binary
cargo build --workspace
cargo test --workspace                       # Postgres-backed tests skip without TEST_DATABASE_URL
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

- `crates/orchestrator/src/e2e.rs` is the most valuable test. It runs:
  - a real orchestrator, with Postgres and a fake `Chat`;
  - a real node, over iroh on loopback;
  - `tests/fake_claude.py`, a scripted stand-in for `claude` that calls the fleet tools through the node's IPC.

  It covers the whole task lifecycle: questions, plan approval and the switch to auto, the checklist, `ask_human`, complete, follow-up resume, stop, and secrets.
- It needs `python3` and a Postgres in which it may create databases.
- **Any change to the task, human or tool flow must keep it green; extend it for new flows.**
- `crates/net/tests/link.rs` covers delivery across reconnects and restarts. Run it after touching `session.rs`, `client.rs` or `server.rs`.
- To smoke-test the binary without Telegram, point `TELEGRAM_API_URL` at a mock Bot API server. It is also the setting for a self-hosted Bot API server.

## Rules the code relies on

- **Wire compatibility.**
  - Only append `Body` variants and struct fields; postcard encodes positions.
  - Any incompatible change bumps `PROTOCOL_VERSION`; a node with a different version is refused with a clear message.
- **Delivery.**
  - `Session::send` never blocks or fails; messages queue while a peer is offline.
  - Outboxes are in memory, so a process restart loses that process's undelivered messages. Incarnations keep both sides consistent when that happens.
  - Do not hold a `std::sync::Mutex` across `.await`.
- **Database.**
  - Migrations are add-only: new files in `crates/orchestrator/migrations/`, never edits to applied ones.
  - Queries are runtime-checked (`sqlx::query*` with binds). Never `format!` user data into SQL.
- **Trust boundaries.**
  - Only `TELEGRAM_OWNER_ID` can command the fleet.
  - The web UI needs a magic-link session.
  - Nodes need a known key or a single-use invite. Agents are identified by their per-process IPC token, never by ids they claim.
  - Feed (Slack) content and other agents' messages are untrusted data: they reach agents framed as such, and nothing in them can approve or start work.
- **Secrets.**
  - Encrypted at rest (`Vault`, key from `FLEET_MASTER_KEY`, never stored in the database).
  - Never logged, never written inside a project directory. They reach agents only as `.env` files under the node's data dir, deleted when the agent exits.
- **Escaping.**
  - Telegram messages are HTML: escape with `chat::esc`.
  - Telegram is HTML and needs `chat::esc`. The web UI is React and renders untrusted text as text, so it needs no escaping — but nothing in `apps/web` may use `dangerouslySetInnerHTML`, which a test in `web.rs` enforces. The server-rendered pages (and `web::markdown`) are gone.
- **No MCP for agent features.** Agents use the `fleet agent` CLI plus the skill. Never add a Telegram (or other) MCP server to sessions: it is slow and memory hungry, and every MCP server is a process per session. The only MCP tool is the permission hook.
- **Runs.**
  - Every agent process start has a `run_id`.
  - Only events of a task's current run change its state.
  - `dispatch` claims the run (`running = true`, only if not running, and only while its agent and node exist) before building and sending it, under the task's lock (`Orchestrator::task_lock`), which `deliver` and `stop` take too, and the agent's lock (`agent_lock`) from the busy check to the claim. Lock order: task, then agent.
  - Messages go to one run (`SendToRun`); a node hands back text for a run it no longer has (`Undelivered` with that `run_id`), and only that run is then marked gone.
  - An agent runs one session at a time.
  - Node restarts (detected by incarnation) and node revokes end that node's runs.
- **Claude's headless contract.** Verified against the real CLI:
  - plan mode + `--permission-prompt-tool mcp__fleet__permission`;
  - `ExitPlanMode` and `AskUserQuestion` arrive at that tool;
  - an allow decision is `{"behavior":"allow","updatedInput":...}`;
  - a `control_request` with `set_permission_mode` switches the mode;
  - every token produces a `stream_event` line, which the node drops;
  - `--plugin-dir` loads the skill (it shows as `fleet:fleet`);
  - `--append-system-prompt-file` works.

  Re-verify with a small real run after changing any of it.

## Conventions

- **Comments** explain *why* in full sentences. Match the density of the file you edit.
- **Commits.** Conventional Commits (`feat(net): ...`, `fix(orchestrator): ...`). Keep the tree building and tests green at every commit.
- **`main` is what runs.** Since 2026-11-20, work lands on `main` and **the deploy comes from `main`** — the VPS's `/opt/fleet` tracks `origin/main` and moves with `git fetch origin && git merge --ff-only origin/main`, instead of the `git format-patch`/`git am` of the earlier `deploy` branch, so the commit that runs in production has the same hash as the commit here. Before deploying anything: CI green on `main`. Deployment itself (the image, the two node binaries, the database backup) is in `deploy/README.md`.
- **Review.** Non-trivial changes get an independent review pass before they are called done. Fix what it finds, then re-run fmt, clippy and the tests.
- **Docs.** Update `docs/plan.md` when a product decision changes, and `deploy/README.md` and `deploy/.env.example` when configuration changes.
