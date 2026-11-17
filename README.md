# Agent fleet

Run a fleet of [Claude Code](https://claude.com/claude-code) agents across your machines and steer it from one Telegram group.

- **Orchestrator** (your VPS, Docker). You write "do X" in Telegram. The orchestrator's brain, itself a Claude Code session, works out the project. It then hands the work to the agent that did similar work before, or creates a new agent with its own Telegram bot identity.
- **Agents** run as `claude -p` sessions on **nodes**: your PCs, and the VPS too.
  1. An agent plans first. You review the plan on a web page, answer its questions there, and approve it.
  2. It then works in its own git worktree and ticks off a milestone checklist in the task's Telegram topic.
  3. It finishes with a short summary.
- **The human queue.** Anything that needs you lands in the project's *needs you* (or *fyi*) topic. You answer with buttons or by replying. Unanswered items get reminders; agents never answer for you.
- **Feeds.** Slack channels you're in are mirrored into project topics through your user token. Agents can read and search them, and the orchestrator can propose tasks or replies. A reply is only posted (as you) after you approve it.
- **Memory.** Hindsight gives every project a long-term memory bank, shared by its agents and the orchestrator.
- **Transport.** Nodes connect to the orchestrator over [iroh](https://iroh.computer): encrypted QUIC that gets through NAT, via your own relay. Each message is delivered exactly once, in order, across reconnects and restarts.

## Quick start

1. Deploy the VPS stack: [`deploy/README.md`](deploy/README.md).
2. In Telegram, send `/invite`. On each PC, install the node with the ticket:
   - **Windows:** `deploy/windows/install-node.ps1`
   - **Linux:** `fleet join <ticket>`
3. Map projects with `fleet project map <slug> <dir>`, add bot tokens on the web **Agents** page (`/web`), and write your first request in the General topic.

## Telegram commands

`/status` · `/queue` · `/tasks` · `/stop [task]` · `/web` · `/invite` · `/brain_reset`

## Develop

```sh
cargo test --workspace
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test --workspace   # + Postgres end-to-end test
```

Design: [`docs/plan.md`](docs/plan.md). Working notes: [`CLAUDE.md`](CLAUDE.md).

## License

MIT
