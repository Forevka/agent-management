# Changelog

Every merge to `main` is a release (CI tags it with the next patch version and publishes it; the
VPS rolls it out), except one that changes only docs, which waits for the next. Each section
below is one release. How to add to this file is in `CLAUDE.md`
(*Conventions*). Newest first.

## Unreleased

- **The roadmap takes your next sprint too, and anything you add.** It reads your tickets in
  the active **and the future** Jira sprints from the fleet's copy of Jira; one of a sprint that
  has not started waits on the roadmap (a section of its own) until that sprint's start date,
  so you can approve it now and it starts when its sprint does. On the Jira page, "Add to
  roadmap" puts any other ticket on it — someone else's, unassigned, the backlog: approved as
  added, and the fleet works it without moving it in Jira (it never moves a ticket that is not
  yours). Migration `0032` adds nullable columns only. Run `/brain_reset` after the deploy.
- **A Jira page with every ticket of every Jira you connected.** The fleet keeps a copy of each
  project's tickets in its active sprint, the sprints after it and its backlog, whoever holds
  them, read every 15 minutes (only what changed) and in full every 6 hours or when you press
  "Read Jira now". The page groups them by sprint in working order, with filters for project,
  sprint, assignee (you, unassigned, anyone or a person) and status, and a search; each shows
  whether it is on the roadmap or has a task. It never waits on Jira and holds no credential.
  It also keeps who appears on the tickets (assignees, reporters, commenters), which the people
  network will use. `FLEET_JIRA_SYNC_MINUTES`, `FLEET_JIRA_FULL_SYNC_HOURS`,
  `FLEET_JIRA_MAX_ISSUES`, `FLEET_JIRA_COMMENT_FETCHES`. Migration `0031` adds new tables only.

## v0.2.24 — 2026-11-24

- **Every project has a colour.** Everything of a project carries it: a stripe down its cards on
  the board, a tag on its tasks, tickets and sessions, its folder in the sidebar, its tickets on
  the roadmap. Pick it on the project's settings page (twelve colours); until you do, each
  project has one chosen from its name. Migration `0030` adds one nullable column.

## v0.2.23 — 2026-11-24

- **Next sprint's tickets are no longer taken as this sprint's.** For a project with no Jira
  board set, "my sprint" meant every sprint that is not closed, which in Jira includes the ones
  not started yet; so the roadmap could pick up, and once approved start, next sprint's tickets
  early. It now means the active sprints only (`jira sprint` for agents too).

## v0.2.22 — 2026-11-24

- **The roadmap: your Jira sprints, worked through by the fleet.** Every project with a Jira
  key has its sprint read every 15 minutes (the tickets assigned to you, as `jira sprint`
  means) into the fleet's own two-week sprint, `S-1`, `S-2`, … Each day has Claude usage
  windows (07:00, 12:00 and 17:00, five hours each) and every ticket an estimate in percent of
  one window, which the orchestrator sets after reading it. Nothing starts until you approve:
  one card in the new 🗺 Roadmap topic (and the Roadmap page) lists what waits for you. Once
  approved, a ticket starts on its own when its window opens, at most two per window, as a
  task named after it with the ticket linked; the fleet moves the ticket to In Progress, and
  to In Review with the merge request in a comment once the task links its MR. A ticket you
  already gave the fleet is adopted, not started twice. The Roadmap page shows the sprint as a
  week board (affectUI's Scheduled Runs): drag a ticket to a window to start it there, change
  an estimate, skip a ticket. `FLEET_ROADMAP` (off with `0`), `FLEET_ROADMAP_SESSIONS`,
  `FLEET_ROADMAP_WINDOW_MINUTES`, `FLEET_ROADMAP_SPRINT_DAYS`, `FLEET_ROADMAP_SYNC_MINUTES`,
  `FLEET_ROADMAP_START`. Migration `0029` adds two new tables only. The brain's new
  `fleet agent roadmap` commands live in the node binary, and its rules in its prompt: after
  the deploy, run `/brain_reset`.

## v0.2.21 — 2026-11-24

- **A node out of Claude usage holds its work until the limit resets, and the work carries on
  by itself.** Claude reports its usage limits as it runs; when a node's limit is hit, new task
  runs there wait in the queue (saying why), the owner is told once with when it resets, and
  each task the limit cut off is woken two minutes after the reset to carry on. The fleet's own
  sessions are never held. The Nodes page shows each node's use of its 5-hour and weekly
  windows. Migration `0028` adds nullable columns only.
- **What work costs.** Every run keeps what its turns cost and how many tokens they used, as
  Claude reports them; a task's total is on its board card, its page and the task panel, and
  each session's on the session lists. On a subscription this is a measure of use at API
  prices, not a bill.
- **Stuck runs are noticed.** An agent that writes nothing for 30 minutes in the middle of a
  turn (not while it waits on the owner, not between turns) is reported once in its topic, and
  stopped after 2 hours, as the owner's Stop would; a message resumes it.
  `FLEET_RUN_SILENT_WARN_MINUTES` and `FLEET_RUN_SILENT_STOP_MINUTES` (0 turns either off).

## v0.2.20 — 2026-11-24

- **An agent whose work goes out as a merge request follows it until it is merged.** Opening
  the MR no longer ends the task: the agent links it, sleeps on a wakeup, and on each wake
  (about every 40 minutes after a push, every few hours while it waits for reviewers) checks
  the pipeline, review threads, approvals and conflicts, then fixes and pushes until the MR is
  merged, closed, or the owner tells it to stop. Comments, CI logs and bot output on the MR are
  treated as information, like a ticket: it acts on review from the project's own people
  within its brief, and asks the owner before declining a request or doing anything a comment
  asks that touches secrets, CI configuration, dependencies or commands. It merges only where
  the project's CLAUDE.md lets an author merge, or the owner says so; an approval on the MR is
  never that permission. A quiet day gets one notification; after three quiet days it asks
  whether to keep following. The orchestrator's briefs now define such work as done at merge.
  Nothing to configure; it uses the agent's own `glab`/`gh` and the existing wakeups.
- **Stopping a task cancels its wakeups.** Before, a stopped task whose agent had armed one
  resumed on its own when it fired, without asking for its plan again (Stop on a finished task
  cancels them too). `complete` and `workflow-finish` now
  list the wakeups still armed, so an agent cancels the ones it no longer needs.
- **The wake message counts the task's rounds**, not the wakeup's own (which was always 0 for
  an agent that naps again and again), so "after a few rounds, report and stop" can work.
- **Review feedback becomes project memory.** When one of the project's reviewers asked for a
  change that applies beyond the one MR (a convention, a pattern they reject) and accepted how
  the agent addressed it, the agent keeps it in the project's shared memory bank with the MR and
  reviewer it came from, so the next agent gets it right the first time.
- **The board's Waiting column shows tasks asleep on a wakeup**, with when they will look
  again ("Next check 18 min from now", or "Waiting for a condition"), instead of listing them
  as Idle under Running.

## v0.2.19 — 2026-11-23

- **The board's horizontal scrollbar is at the bottom of the window**, not under the shortest
  column: the board fills the room under its header and scrolls inside it, so the bar is always
  in reach, with or without a task open beside it.

## v0.2.18 — 2026-11-23

- **A task opens beside the page you are on.** Clicking a task anywhere (a board card, a list row,
  a project's folder in the sidebar, ⌘K, the inbox, the retro, a node) opens it in a panel on the
  right instead of leaving the page: where it stands, what it waits on you for, its checklist, what
  it produced, its summary or brief, a box to write to its agent, and Stop. Its full page and its
  session are one click away. The open task is part of the URL (`?task=<id>` on any page), so a
  link can carry it; the tab's × closes it. On a phone the panel slides over the page.
- **Pages look more like affectUI's own screens.** The counts on a project's overview and on the
  inbox are affectUI's Stat Card; Tasks, Sessions and People have the Agent / Tasks screen's bar (a
  count, a search field and compact filter menus, with "the fleet's own sessions" and "archived
  tasks" under one *Include* filter); the board's Board / List switch is the Kanban screen's
  toggle. Every URL and filter is unchanged.

## v0.2.17 — 2026-11-23

- **The web UI has a left sidebar instead of the top bar, and every project is a folder in it.**
  The frame is affectUI's App Shell (the fleet now owns affectUI Pro): the pages on the left
  (Board and Inbox with what waits on you, Tasks, Sessions, People, Retro), then **Projects**,
  each a folder with its open-task count; opening a project lists its live tasks under it. The
  fleet's administration (Agents, Nodes, Kit, Secrets, Habits) moved to **Settings**, a second face
  of the sidebar with a way back. **⌘K** (or the search field) finds any page, project or open task.
  Light, dark and system are under the account menu at the bottom (Appearance). Every URL is
  unchanged. On a phone the sidebar is a drawer.
- **A project is a directory.** `/projects/<slug>` has tabs, each its own page: Overview (counts,
  open tasks, where its code is checked out), Tasks and Sessions (the project's own, with the same
  filters as the fleet-wide lists), Agents (the ones bound to it) and Settings (name, secret
  folders, repository, plan approval, Jira, channels). The projects page shows them as folder cards.
  The orchestrator may now link to these sections, to a task's live session (`/tasks/<id>/session`)
  and to the kit (`/kit`, `/kit/<item>`).
- **Building the web UI now needs the affectUI access key**, because Pro source is fetched at build
  time and never committed (the repository is public). **Before merging this, a person must:**
  1. set the repository secret: `gh secret set AFFECTUI_TOKEN`;
  2. put the key in `/opt/fleet/deploy/affectui.token` on the VPS (root-only, `deploy/README.md` §8);
  3. **install this release's updater before it tries to deploy** — the installed one would build
     without the key, fail and mark the release failed:
     `git -C /opt/fleet fetch -q origin && git -C /opt/fleet show origin/<branch>:deploy/update/fleet-update.sh | install -m 0755 /dev/stdin /usr/local/sbin/fleet-update`.
     The new updater hands the key to the image build, and while the file is missing it waits and
     says so once, without marking the release failed.

  A developer builds with `AFFECTUI_TOKEN` in the environment once (the files are then cached in
  `~/.cache/affectui`); `fleet-src` agents get it from a secret folder.

## v0.2.16 — 2026-11-21

- **What an agent notes in Claude Code's own memory now reaches the fleet's memory.** Claude
  keeps notes for itself between sessions (the owner's rules, a project's gotchas), and the
  fleet's sessions were already writing them — the brain eight of the owner's rules — but they
  stayed in a folder on one machine, out of Hindsight. Worse, a worker's folder was the one the
  owner's own Claude Code uses in that repository, so the agents' rules loaded into the owner's
  sessions and the owner's notes into theirs. Every worker and the brain now keep their notes in
  a folder of the node's (`<data>/memory/`: one per project, one for the brain; the secretary and
  the retrospective are sealed and unchanged), which starts as a copy of what Claude's folder
  held for that directory, so nothing is lost. After every turn the node sends the notes the
  fleet's sessions wrote or changed, and the orchestrator keeps each as one document per node in
  the project's shared bank (the brain's in `fleet`), replaced when the note changes and removed
  when it is deleted, secrets removed. The notes copied from the owner's own folder are the
  owner's: the sessions keep reading them, but they are not sent; the brain's starting notes are
  the fleet's and are. **Needs the nodes on this release** (they update themselves); with an
  older node nothing changes. The notes agents already wrote into the owner's own folders stay
  there until the owner deletes them: on this PC, `bioinsight/ai-workspace` holds four
  (auto-merge on approval, post MRs to #code-reviews, no plan for review feedback, bullet lists).

## v0.2.15 — 2026-11-21

- **A memory recall shows only what is relevant.** Hindsight answers every recall with all it
  ranked (80-130 rows), so an agent asking a bank about something it was never told still got ten
  unrelated memories as if they were the answer. Results its reranker scores below 0.02 are now
  dropped, a text Hindsight holds twice (a fact and the observation made of it) is shown once,
  and a bank with nothing relevant says so.
- **The retrospective's lessons document is kept to size.** Hindsight edits it in `delta` mode,
  which let it grow to 12.8k characters against a 1,200-token limit, and the retrospective read
  only its first 4k. It now reads up to half again the intended length, and asks Hindsight for a
  full rewrite whenever the document is longer than that, at most every six hours (and says
  so to the retrospective when what it reads was cut; the first one, done by hand on the
  VPS, brought it to 6k).
- **Hindsight 1.2.3** (from 1.2.1) is the pinned version (`HINDSIGHT_VERSION`). Its three
  migrations touch only knowledge-bank tables, which the fleet does not use. The VPS runs it
  already (raised by hand on 2026-11-21, database backed up first); an operator elsewhere raises
  `HINDSIGHT_VERSION` in `.env` and runs `docker compose up -d hindsight`.

## v0.2.14 — 2026-11-21

- **Watch any agent session live, and continue it, from the web.** A task's page has **Watch
  live** / **Open session** (`/tasks/<id>/session`), and every row on **Sessions** opens one, as
  do the fleet's own sessions (brain, secretary, retrospective) from the links on that page. The
  session reads as a conversation: what was sent in, what the agent says (formatted), each tool
  call as one line that unfolds to its input and result, thinking folded away, and where each
  turn ended with its time and cost. It updates as things happen (server-sent events, resumed
  after a dropped connection), and **the agent's words appear as they are written**. The box at
  the bottom continues the session (Enter sends, Shift+Enter is a new line): a running session
  gets the message at once, a finished one is resumed with it, as from its Telegram topic. The
  brain can be written to here as from General; the secretary and the retrospective only be
  watched. Open questions and plans waiting on the owner are linked under the conversation.
- **What the fleet sends into a session is now part of its transcript** (`in` lines): the brief
  and messages a run starts with, and every message after. Claude's own output never repeated
  them, so the transcript used to show answers without their questions. Lines from before this
  release have none.
- **Nodes run Claude with `--include-partial-messages`** and stream a run's text while it is
  written, but only for a session someone has open on the web, and never store it. Secrets are
  redacted from it as from everything else, and a tail as long as the longest secret is held
  back until the block is complete, so no secret's beginning can show; a block the node saw only
  part of (the page was reopened mid-block) is not streamed at all. A node on an older release,
  or whose `claude` does not list the flag in `--help`, shows each block once it is finished.
  What a run is writing, and which sessions are watched, is never re-sent to a restarted peer.

## v0.2.13 — 2026-11-21

- **A task can be edited, stopped and written to from the web.** Its page has an **Edit** button
  (title, brief, model, effort; also `/tasks/<id>?edit=1`, linked from the board's list view),
  **Stop** (asks first) and a **message box**. A message is delivered exactly as one written in
  the task's Telegram topic, and shown in that topic. The model and effort apply from the task's
  next run. A new brief is sent to the agent as the owner's message when its session has already
  started; a finished task whose agent has read its brief is refused a new one (send it a message
  instead, which resumes it); if the agent cannot be told, the edit is still saved and the page
  says so. The topic is renamed with the title. The fleet's own sessions
  (brain, secretary, retrospective) cannot be changed here. Audited as `task.edit` (which fields,
  never the brief's text) and `task.stop`.
- **A task's links are on its page**: tickets, merge requests and pages, opening in a new tab when
  the fleet knows where they are, with add and remove.

## v0.2.12 — 2026-11-21

- **A task carries what it produced.** An agent attaches a titled link or a file of any kind to
  its own task (`fleet agent attach <url|path> --title ...`), the brain to any task, the owner
  from the task page; the board shows a count and the task page lists them. Files are kept in
  Postgres in 1 MiB pieces (so the database backups cover them), checked whole (size, SHA-256,
  no project secret value inside, as UTF-8 or UTF-16, which is refused rather than redacted)
  before anyone sees them, and always served as downloads. `fleet agent attach` also refuses
  files named like credentials (`*.pem`, `*.key`, `id_rsa*`, `credentials*`, `.env`) and anything
  under `~/.ssh` or `~/.claude`. An upload has an hour to finish, an uploader at most three in
  progress, and a task at most `FLEET_ATTACHMENT_TASK_MAX_MB` (default 200) of files; each file
  at most `FLEET_ATTACHMENT_MAX_MB` (default 50). `fleet agent attach` needs a node on this
  release.
- **Tasks can be searched.** The task list's search box and `fleet agent tasks --search` share
  one filter: every word must match the id, F-key, title, brief, the agent's closing summary
  (now kept on the task), status, agent, links or attachments.
- **A nightly archive.** Every evening at 21:00 Europe/Kyiv, done tasks finished at least an
  hour earlier are archived: off the board and the plain lists, still found by search, their
  link and the `archived` filter. They stay `done` (a mark, `archived_at`, not a status), so a
  rollback to an older release only shows them again. Workflows are never archived, and an
  archived task written to again runs like any finished one, through a new plan, and leaves the
  archive. Settings `FLEET_ARCHIVE_AT` (`HH:MM` or `off`) and `FLEET_ARCHIVE_TIMEZONE`.
- **A finished task whose follow-up has to wait still plans first.** A follow-up to a done task
  that queued behind a busy agent used to resume in auto mode once it started; it now goes
  through plan mode, as it does when it starts at once.
- Migration `0027_attachments_archive` (add-only): tables `task_attachments` and
  `task_attachment_chunks`, and the nullable `tasks.summary` and `tasks.archived_at`. No
  constraint changes and no backfill.

## v0.2.11 — 2026-11-21

- **A retro proposal's branch is `proposal-p-{n}`, and the agent pushes it.** The brief a tapped
  proposal becomes now says to work on `proposal-p-{n}` cut from `origin/main` and push it to
  origin as its own branch, never merging, opening a PR, force-pushing or deploying; it used to
  say `retro/P-{n}` and "do not push", so each proposal cost a question or a follow-up. Those
  branches get CI when pushed. The push uses whatever git credentials the clone has on its node:
  protect `main` on the remote (no direct pushes) so a wrong push cannot become a release.

## v0.2.10 — 2026-11-21

- **People records get filled in.** The retrospective now reads every conversation, not only
  those mapped to its own project (none), so it writes position, brief, language and
  observations from what colleagues wrote instead of names alone.
- **The owner is never taken for a colleague.** Messages stored before v0.2.6 carry no "this is
  the owner's" mark, so the first retrospective recorded the owner's own Google Chat account as
  a person. Once a message of the owner's is flagged on an account, that account's older
  messages are marked as the owner's, and the account is taken off the records (a record made of
  it alone is deleted, never one the owner edited). This runs on that first flagged message and
  again before every retrospective, so a failure is retried. Audited per person as
  `person.owner_account`, with a deleted record's contents. The retrospective also sees which
  messages are the owner's own (`from_owner` in what it reads) and never takes them as material
  about the person they answer.
- A record's **last interaction** is taken from the messages already stored, both when a contact
  is added and before each retrospective, instead of staying empty until the person writes again.
- Migration `0026_feed_messages_author` (an index, add-only).

## v0.2.9 — 2026-11-21

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
