# Changelog

Every merge to `main` is a release (CI tags it with the next patch version and publishes it; the
VPS rolls it out), so each section below is one merge. How to add to this file is in `CLAUDE.md`
(*Conventions*). Newest first.

## Unreleased

- **Changelog.** This file: what changed in each release, written with the change itself.

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
