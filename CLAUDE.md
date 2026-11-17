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
| `crates/node` | Node role: `runner.rs` starts and supervises Claude (stream-json in and out, permission prompts through the fleet MCP tool, plan approval switches the session to `auto` with a `set_permission_mode` control request). `ipc.rs` is loopback TCP with a token per agent. `mcp.rs` is the `fleet mcp` stdio shim. `tools.rs` holds tool schemas. `secrets.rs` writes `.env` files outside the project. `config.rs` is `node.json` (project mappings, settings). |
| `crates/orchestrator` | `core.rs` handles node events. `tasks.rs` creates tasks, places them on nodes, delivers messages and runs the brain. `human.rs` is the human queue: questions, plans, permissions, proposals. `tools.rs` implements the fleet tools. `tg.rs` is the Telegram `Chat` implementation and update loops. `web.rs` is axum, with magic links. `auth.rs` decides node admission. `feeds.rs` covers Slack ingestion and the feed tools. `vault.rs` encrypts at rest. `prompts.rs` holds role prompts. `migrations/` is the SQL schema. `e2e.rs` is the end-to-end test. |
| `crates/telegram` | Thin, typed Bot API client: rate limiting, retries, HTML and callback helpers. |
| `crates/feeds` | Source-agnostic feed model plus `SlackSource`, which polls the Web API with the owner's **user token**. There is no Slack app, no Events API and no Socket Mode. |
| `crates/fleet` | The CLI: `orchestrator`, `invite`, `join`, `node`, `project map`, `secret get/exec`, `mcp`, `id`. |
| `deploy/` | Docker Compose stack: orchestrator, Postgres with pgvector, Hindsight, iroh-relay, Caddy. Also the systemd unit, the Windows installer and backups. |
| `.github/workflows/` | CI (fmt, clippy, tests with Postgres, Windows build) and release (Linux and Windows binaries, GHCR image). |

## Build and test

```sh
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
  - The web UI escapes everything with `esc`; Markdown from agents goes through `web::markdown`, which turns raw HTML into text.
- **Claude's headless contract.** Verified against the real CLI:
  - plan mode + `--permission-prompt-tool mcp__fleet__permission`;
  - `ExitPlanMode` and `AskUserQuestion` arrive at that tool;
  - an allow decision is `{"behavior":"allow","updatedInput":...}`;
  - a `control_request` with `set_permission_mode` switches the mode;
  - every token produces a `stream_event` line, which the node drops.

  Re-verify with a small real run after changing any of it.

## Conventions

- **Comments** explain *why* in full sentences. Match the density of the file you edit.
- **Commits.** Conventional Commits (`feat(net): ...`, `fix(orchestrator): ...`). Keep the tree building and tests green at every commit.
- **Review.** Non-trivial changes get an independent review pass before they are called done. Fix what it finds, then re-run fmt, clippy and the tests.
- **Docs.** Update `docs/plan.md` when a product decision changes, and `deploy/README.md` and `deploy/.env.example` when configuration changes.
