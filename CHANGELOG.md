log

Every merge to `main` is a release (CI tags it with the next patch version and publishes it; the
VPS rolls it out), except one that changes only docs, which waits for the next. Each section
below is one release. How to add to this file is in `CLAUDE.md`
(*Conventions*). Newest first.

## Unreleased

- **Docs-only changes make no release.** A push or merge that changes only Markdown files and
  `docs/` runs no CI, so nothing is tagged or rolled out to the nodes; it ships with the next
  release. Branches named `docs/**` now get CI like the other work branches.
- `CLAUDE.md` asks everyone working on the repo (people and agents alike) to delete build
  folders and remove merged worktrees and clones when done. Each `target/` is 7-40 GB.

## v0.2.8 — 2026-11-21

- **The kit.** The owner's Claude Code skills, plugins and MCP servers, managed on the new
  **Kit** page, imported from a node (an MCP server's whole config sealed in the vault, shown
  only masked, with nothing of it in Postgres but its type and command or host), and
  installed on every node at user scope — never over a skill or MCP server of the machine's own,
  and removing only what the kit installed. A per-node matrix shows installed, failed, conflict
  or removed. Only the owner changes it; the brain cannot, not even by proposal.
- **Agent sessions get exactly the kit's MCP servers.** Every session now runs with
  `--strict-mcp-config`: the fleet's permission server plus the kit's enabled MCP servers
  (workers only; the brain, the secretary and the retrospective get none), from a per-run config
  under the node's data dir that is deleted when the run exits. MCP servers configured on a
  machine some other way no longer load in agent sessions — add the ones agents need to the kit.
- The allowlist needs a claude CLI with `--strict-mcp-config` (2.1.268 verified). On an older one
  a node starts its sessions without it, and the Kit page says the allowlist is not enforced there
  until the CLI is updated.
- New messages (`KitCaptureRequest`, `KitCapture`, `Kit`, `KitStatus`), sent only to nodes level
  with the orchestrator's release; no protocol bump. Migration `0025_kit` (two new tables and a
  nullable `nodes.kit_note`).

## v0.2.7 — 2026-11-21

- **Projects assigned to a node, cloned on demand.** A project records its git URL; a URL with
  credentials, a query string or a fragment in it is refused. On a node's page the owner assigns
  projects to it and may set where clones go (default `~/fleet-projects`); only the owner can, and
  each change is audited. A task for an assigned project can be placed on that node, after nodes
  that already have it mapped. The node clones it with the machine's own git login and maps it,
  then starts the run; messages sent to the run meanwhile are delivered once it starts.
  - Git never prompts: no terminal prompt, no askpass, no credential-manager dialog. ssh runs in
    batch mode unless the machine set its own ssh command (`GIT_SSH_COMMAND`, `GIT_SSH` or
    `core.sshCommand`), which is left exactly as it is. One clone per project runs at a time,
    stopped after 20 minutes together with every process it started.
  - The clone is made in a temporary `.fleet-clone-<slug>-*` directory and renamed into place
    only when git succeeds; the node removes only such directories of its own. A finished checkout
    of the same repository already at the target is adopted; anything else is left alone, and
    unassigning deletes nothing.
  - The node refuses to clone onto a volume with less than 1 GB free.
  - Stopping a waiting task stops its git. A run that never starts hands its messages back to the
    task's queue; a stopped task stays stopped, and they go with its next run.
  - A failed clone fails the run with what to do, and the assignment takes no more tasks until
    the owner presses **Retry** (or changes the URL, the root or the assignment); the task's topic
    says so.
  - A node clones only once it runs this release. Migration `0024_node_projects` (add-only).

## v0.2.6 — 2026-11-21

- **People network.** The fleet keeps records of your colleagues (position, what they work on,
  the company's projects, how to reach them, language, observations, and tags for anything else).
  The daily retrospective creates and updates them after its review, without pinging you, at most
  `FLEET_RETRO_MAX_PEOPLE` (30) writes a run; you review, edit and delete them on the new People
  page. A field you set stays yours: the retrospective can add to your observations, tags and
  contacts but never changes or removes what you set, and never puts back what you removed. The secretary gets the authors' cards with
  its batches, the orchestrator one line per known author, and both look people up
  (`fleet agent people`) before writing to them — a card shapes language, tone and form of
  address only, never what the fleet does. Migration `0023_people.sql` adds two new tables and a
  nullable `feed_messages.from_owner` column (with a small partial index), which keeps you out
  of the records.

## v0.2.5 — 2026-11-21

- **Changelog.** This file: what changed in each release, written with the change itself.
- The Windows installer waits for a binary that was just stopped to let go of its file (the tray
  holds it a few seconds), instead of failing halfway through a reinstall.

## v0.2.4 — 2026-11-20

- **Windows nodes update themselves.** A node on v0.2.4 or later that connects behind the
  orchestrator's release is told which one to move to. It downloads the binary only from the
  GitHub repository it was built from (https, `SHA256SUMS`, `--version` checked), retries while
  the release's files are still being published, waits for its agents (two hours, a day while
  one waits on the owner's approval), swaps itself and restarts. The launcher puts the previous
  binary back if the new one keeps dying at startup (and never takes that release again) or never
  reaches the orchestrator. Only under the launcher (`FLEET_SELF_UPDATE=1`); agent sessions never
  inherit it. A node refused for an older protocol finds the release to update to in the refusal.
  **Nodes on v0.2.3 or older need one reinstall with the current installer** to get here.
- **Disk space on nodes** (retro P-3). Nodes report free space where Claude Code and the projects
  write; below `FLEET_DISK_LOW_GB` the owner is warned (once, with sound, at most every 12 hours
  for the same state), below `FLEET_DISK_FLOOR_GB` new task runs on that node are held until
  there is space again. The fleet's own sessions are never held. Shown in `/nodes`, a node's page
  and `/status`.
- **Standing duties become habits** (retro P-4). A duty the owner states ("every day review two
  MRs") is proposed as a `do` habit at once; every run, skip and failure is recorded and shown on
  the Habits page and in `/habits`, and the brain reports what it did with each request
  (`fleet agent habit report`). A duty cannot be added twice on the same schedule and project, or
  in the fleet's own project.
- No protocol bump: the orchestrator sends its new messages only to nodes that can read them, and
  a node reports its disk only when asked, so older nodes and a rolled-back orchestrator keep
  working.
- Migrations `0021_node_disk`, `0022_habit_log` (add-only).

## v0.2.3 — 2026-11-20

- **Release guard.** Only a tag on `main` is released; a tag pushed elsewhere builds nothing.
- **`deploy/NODE-SETUP.md`**: a runbook for the Claude Code session setting up a new node, which
  asks the owner for what only they have (the invite ticket, which projects).

## v0.2.2 — 2026-11-20

- **Rollback works**: the migrator ignores applied migrations it does not know, and new columns
  are nullable or defaulted, so the previous release runs against the newer schema; a failed
  compose start restores the previous image; the dry run refuses a dirty or altered migration
  history and waits at most 5 s for a lock.
- The updater deploys only tags on `main`, puts the checkout back after any failure, and does not
  retry a release that failed.

## v0.2.1 — 2026-11-20

- **The VPS rolls out each release itself** (`fleet-update.timer`, every 5 minutes): builds the
  tag's image, dry-runs its migrations against the live database in a rolled-back transaction
  (`fleet migrate --dry-run`), backs up, swaps, requires 20 s of health and rolls back
  otherwise; then the VPS node, once idle.

## v0.2.0 — 2026-11-20

The first release: everything built from 2026-11-17 on, in one.

- **Core.** One `fleet` binary: the orchestrator (Postgres, Telegram forum group, web UI,
  Hindsight memory) and nodes that run `claude -p` sessions, linked over iroh with exactly-once
  delivery across reconnects and restarts. Every binary reports its release (`fleet --version`).
- **Tasks.** Plan approval, questions, checklists, follow-ups, stop; short task keys (`F-41`);
  links to what a task is about outside the fleet (tickets, merge requests, URLs); a project
  grows another agent instead of queueing its work (up to `FLEET_MAX_AGENTS_PER_PROJECT` per
  role), and asks the owner for a managed bot when the pool is dry; model and effort per agent and
  per task (default `claude-opus-5-5`).
- **The brain** administers the fleet with the owner's approval; **the secretary** triages every
  incoming message (Slack over Socket Mode, Google Chat) into the Inbox, todos and rules;
  **workflows** are standing automations on a schedule; **wakeups** let an agent wait without a
  session; **habits** learn what the owner keeps deciding; **the retrospective** reviews the
  fleet's day and proposes improvements, with a memory across days (Hindsight mental models).
- **Plan gate** per workflow and project, set by the owner only.
- **Jira** per project, the credential held by the orchestrator: agents read their own ticket.
- **Web UI** (React, affectUI): a board of the fleet's work as the landing page, every page a
  real URL, retro and habits pages.
- **Nodes**: the Windows installer and tray app for mapping projects; finished topics are deleted
  once quiet.
