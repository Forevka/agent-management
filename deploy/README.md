# Deploying the fleet

What runs where:

| Where | What | How |
|---|---|---|
| VPS, Docker Compose (`deploy/docker-compose.yml`) | `orchestrator`, `postgres` (pgvector), `hindsight` (memory), `iroh-relay`, `caddy` (HTTPS) | `docker compose up -d` |
| VPS host, outside Docker | `fleet node` (also hosts the brain Claude session) | systemd: `deploy/systemd/fleet-node.service` |
| Each Windows PC | `fleet node` | Scheduled Task at logon: `deploy/windows/install-node.ps1` |

Nodes run outside Docker because they start Claude Code with the user's own
login, git credentials and repositories.

Ports on the VPS:

| Port | Service | Public? | Purpose |
|---|---|---|---|
| 22/tcp | sshd | yes | administration |
| 80/tcp | caddy | yes | ACME challenges, redirect to HTTPS |
| 443/tcp | caddy | yes | web UI (`FLEET_DOMAIN`), relay (`RELAY_DOMAIN`) |
| 443/udp | caddy | optional | HTTP/3 |
| 7700/udp | orchestrator | yes | iroh QUIC: nodes connect here |
| 7842/udp | iroh-relay | yes | QUIC address discovery (iroh 1.x replaced STUN/3478 with this) |
| 8080/tcp | orchestrator | no (Docker network) | web UI, reached through Caddy |
| 8888, 9999/tcp | hindsight | 127.0.0.1 only | memory API (used by the orchestrator), control-plane UI |
| 5432/tcp | postgres | no (Docker network) | |

## 1. Prerequisites

- A VPS with Debian 12/13 or Ubuntu 24.04, a public IPv4 address, at least
  **4 GB RAM** (Hindsight's full image runs local embedding and reranker
  models: about 1.5-2 GB on its own) and 30 GB of disk.
- Docker Engine with the Compose plugin
  (<https://docs.docker.com/engine/install/>). Check: `docker compose version`.
- A domain you control, an Anthropic API key for Hindsight, and a Telegram account.

## 2. DNS

Create two records pointing at the VPS's public IPv4 address:

```
fleet.<domain>   A   <VPS IPv4>
relay.<domain>   A   <VPS IPv4>
```

Add `AAAA` records only if the VPS has working IPv6 and Docker publishes on it
(the default). The relay's QUIC address discovery listens on IPv4 only in this
setup. Wait until `dig +short fleet.<domain>` and `dig +short relay.<domain>`
return the IP: Caddy cannot get certificates before that.

## 3. Firewall

```sh
sudo ufw allow 22/tcp
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw allow 443/udp
sudo ufw allow 7700/udp
sudo ufw allow 7842/udp
sudo ufw enable
```

Do the same in your provider's cloud firewall if it has one.

> **Docker bypasses ufw.** Ports published by Docker are reachable even
> without a ufw rule. That is why the compose file publishes Hindsight on
> `127.0.0.1` only and Postgres not at all. Do not change those bindings to
> `0.0.0.0`.

## 4. Telegram (before the first start)

1. **Orchestrator bot.** In [@BotFather](https://t.me/BotFather): `/newbot`, and
   keep the token (`TELEGRAM_ORCHESTRATOR_TOKEN`). Then `/setprivacy` → choose
   the bot → **Disable**, so the bot sees every message in the group.
2. **Group.** Create a group, then in its settings turn on **Topics** (this
   makes it a supergroup with forum topics).
3. Add the bot to the group and **promote it to admin** with at least
   *Manage Topics*, *Delete Messages* and *Pin Messages*. *Delete Messages* is
   what lets it delete a finished task's topic once that topic goes quiet
   (`FLEET_TOPIC_DELETE_AFTER_MINUTES`, an hour by default); without the right
   it says so once and leaves every topic alone.
4. **Chat ID and your user ID.** Send any message in the group, then:

   ```sh
   curl -s "https://api.telegram.org/bot<TOKEN>/getUpdates" | python3 -m json.tool
   ```

   - `message.chat.id` (starts with `-100`) → `TELEGRAM_CHAT_ID`
   - `message.from.id` (your account) → `TELEGRAM_OWNER_ID`

   Do this before starting the orchestrator: `getUpdates` returns nothing (or
   a 409 conflict) while another client is polling the same bot. If the result
   is empty, send another message and retry.
5. Agent bots (the bot pool) are created the same way and added to the group;
   their tokens are entered in the web UI later, not in `.env`. One is enough to
   start: when the fleet needs another and the pool is empty, it asks you in a
   private chat with a button that creates the bot and hands it over, so you
   never copy a token (see "Bot pool" in `docs/plan.md`). For that to work, open
   a private chat with the orchestrator bot once (`/start`), and leave **bot
   management** enabled for it in the @BotFather Mini App — `getMe` shows it as
   `can_manage_bots`. Without either, agents simply speak as the orchestrator.

## 5. Configure and start the stack

```sh
sudo git clone https://github.com/Forevka/agent-management.git /opt/fleet
sudo chown -R "$USER" /opt/fleet
cd /opt/fleet/deploy
cp .env.example .env
chmod 600 .env
```

Fill in `.env`. Generate the secrets with:

```sh
for v in FLEET_MASTER_KEY HINDSIGHT_API_KEY HINDSIGHT_CP_ACCESS_KEY; do
  echo "$v=$(openssl rand -hex 32)"
done
for v in POSTGRES_PASSWORD FLEET_DB_PASSWORD HINDSIGHT_DB_PASSWORD; do
  echo "$v=$(openssl rand -hex 24)"
done
```

**Save `FLEET_MASTER_KEY` somewhere offline** (a password manager). It
encrypts stored secrets; database backups are useless without it.

Then:

```sh
docker compose config --quiet   # validates .env and the compose file
docker compose up -d --build
docker compose ps
docker compose logs -f orchestrator
```

The first start takes a while: the orchestrator image is compiled (several
minutes), Hindsight pulls a ~2 GB image and runs its migrations, and Caddy
requests the certificates. **The `iroh-relay` container restarts until Caddy has
the relay's certificate**; that is expected and stops within a minute or two
(`docker compose logs iroh-relay` then shows `QUIC server listening` and
`Reloaded the certificate`).

Check:

```sh
curl -I https://fleet.<domain>/                     # web UI through Caddy
curl -s -o /dev/null -w '%{http_code}\n' https://relay.<domain>/ping   # 200
curl -s -H "Authorization: Bearer $HINDSIGHT_API_KEY" http://127.0.0.1:8888/health
```

## 6. Join a node

**Setting one up with Claude Code?** Give the session on the new machine
[`deploy/NODE-SETUP.md`](NODE-SETUP.md) — for example: *"Read
https://raw.githubusercontent.com/Forevka/agent-management/main/deploy/NODE-SETUP.md and follow it."*
It is written for an agent: it detects the OS, installs the newest release after checking its
sums, and stops to ask you for the ticket, which projects to map and where they live.

Every node needs a one-time ticket from the orchestrator (or send `/invite` in the Telegram group):

```sh
cd /opt/fleet/deploy
docker compose exec orchestrator fleet invite
```

Get the node binary from the GitHub release (`fleet-<version>-x86_64-unknown-linux-gnu`
or `fleet-<version>-x86_64-pc-windows-msvc.exe`) and check it against
`SHA256SUMS` from the same release.

On each node, first log in to Claude Code as the user the node will run as
(`claude`, then `/login`, or set a token later in the web UI), and check that
`claude -p "say hi"` works.

> `fleet join` may install and start the node service by itself once that is
> implemented. The steps below are the explicit way; use one or the other, not
> both.

### The VPS host (Linux, systemd)

```sh
U=<user logged into Claude Code>
sudo install -m 0755 fleet-*-x86_64-unknown-linux-gnu /usr/local/bin/fleet
# `fleet join` keeps running as the node after joining. Stop it (Ctrl+C) once
# it logs "connected to orchestrator"; the service below takes over.
sudo -u "$U" env FLEET_DATA_DIR=/home/$U/.local/share/fleet FLEET_RELAY=https://relay.<domain> \
  fleet join <ticket>
sudo install -d -m 0755 /etc/fleet
sed "s/CHANGE_ME/$U/g" deploy/systemd/node.env.example | sudo tee /etc/fleet/node.env >/dev/null
sudo chown root:"$U" /etc/fleet/node.env && sudo chmod 0640 /etc/fleet/node.env
sudoedit /etc/fleet/node.env          # FLEET_RELAY, PATH
sed "s/CHANGE_ME/$U/g" deploy/systemd/fleet-node.service | sudo tee /etc/systemd/system/fleet-node.service >/dev/null
sudo systemctl daemon-reload
sudo systemctl enable --now fleet-node
journalctl -u fleet-node -f
```

The VPS node also hosts the orchestrator **brain** (the Claude session you talk
to in the General topic). Give it an empty working directory:

```sh
sudo -u "$U" mkdir -p /home/$U/fleet-brain
sudo -u "$U" env FLEET_DATA_DIR=/home/$U/.local/share/fleet fleet project map fleet /home/$U/fleet-brain
```

> **Where the brain runs matters.** The brain's changes wait for your Approve
> tap, but that gate only holds inside the orchestrator. On the machine itself
> the brain is a Claude session with a shell, running as the node's user:
>
> - The node's user must **not** be able to read `/opt/fleet/deploy/.env`
>   (it holds `FLEET_MASTER_KEY`, the orchestrator bot's token and the database
>   passwords), and must **not** be in
>   the `docker` group (that is root on the host, with the database and the
>   orchestrator's data). Keep `.env` owned by another user, mode `0600`.
> - Every run on a node shares that one OS user, so the brain can read other
>   runs' `.env` files and the node's `node.json` while they exist. Map only the
>   `fleet` project on the brain's node, or at least no project that has secret
>   folders bound, so there is nothing of value to read there.
> - Secret values are redacted from what agents send on (summaries, questions,
>   posts). That guards against carelessness, not against a brain or agent that
>   is trying to leak them.

Memory needs no setup on nodes: agents call `fleet agent memory ...` (taught by
the `fleet` skill the node loads into every session), and the orchestrator
forwards those calls to Hindsight. Each agent owns the banks under its prefix
`agent-<agent-id>`, a project's agents share `project-<slug>`, and the brain
may use any bank.

### A Windows PC

In a normal (not elevated) PowerShell, as the user logged into Claude Code:

```powershell
powershell -ExecutionPolicy Bypass -File .\deploy\windows\install-node.ps1 `
  -FleetExe .\fleet-<version>-x86_64-pc-windows-msvc.exe `
  -TrayExe .\fleet-tray-<version>-x86_64-pc-windows-msvc.exe `
  -Relay https://relay.<domain> -Ticket <ticket>
```

This copies `fleet.exe` to `%LOCALAPPDATA%\Programs\fleet` (added to the user
PATH), joins, and registers the Scheduled Task **fleet node** that starts at
logon and restarts the node whenever it exits. Logs:
`%LOCALAPPDATA%\fleet\logs\node.log`. The script's header explains why it is a
logon task and not a service. The node runs only while the user is logged on;
locking the screen is fine.

Remove it with `install-node.ps1 -Uninstall` (add `-RemoveData` to also delete
the node's key and state).

#### The tray app

`-TrayExe` (optional) installs `fleet-tray.exe` next to `fleet.exe`. While the
node runs it shows a tray icon; click it to map projects to folders on this PC
instead of running `fleet project map`. The project dropdown lists the
orchestrator's projects, kept current by the node, and mappings the
orchestrator does not know are flagged. Changes apply within 5 seconds, without
a restart.

- `fleet node` starts the tray once at startup if `fleet-tray.exe` sits next
  to it. The tray exits when the node stops. **Quit tray** in its menu closes it
  until the node next starts; set `FLEET_NO_TRAY=1` (user environment) to never
  start it. The node reads the variable when it starts, so restart the node
  (or log off and on) after changing it. `fleet join` never starts the tray.
- The node writes `status.json` (link state, the orchestrator's projects, its
  pid and start time) in its data directory for the tray. The tray edits only
  the `projects` key of `node.json`, under the same `node.json.lock` the node
  and `fleet project map` take; the node's settings in it are left as they are.
- The tray logs problems it cannot show in its window to
  `<data dir>\logs\tray.log` (the data dir is `%LOCALAPPDATA%\fleet\data` for
  an installed node).
- Without `-TrayExe`, the script picks up `fleet-tray.exe` or a release asset
  `fleet-tray-<version>-x86_64-pc-windows-msvc.exe` sitting next to `-FleetExe`
  (the newest, if there are several). Building it yourself: see
  `apps/tray/README.md`.


### After the first node: projects, bots, Claude

1. In Telegram, send `/web` to get a login link for the web UI.
2. **Agents** page: register the agent bots' tokens (each bot must already be a
   member of the group).
   The same page sets each agent's default Claude model and effort (empty: the
   fleet's `FLEET_MODEL` and Claude's default effort). The orchestrator may pick a
   model and effort per task only from `FLEET_ALLOWED_MODELS` and
   `FLEET_ALLOWED_EFFORTS` (see `deploy/.env.example`); a node whose Claude command
   sets `--model` or `--effort` keeps its own.
3. **Projects** page: create each project (its slug is what nodes map), bind
   secret folders, add Slack channels.
4. On each host, map the project: `fleet project map <slug> <directory>`, or
   on Windows, from the tray app.
5. **Nodes** page: per node, set the Claude command (e.g. a full path to
   `claude.exe`) and authentication (the host's own login, a
   `claude setup-token` token, or an API key).
6. **Secrets** page: a folder named `slack` with `SLACK_USER_TOKEN` turns on
   the Slack feed (see "Slack and the secretary" below for the app that lets
   it see everything). A folder named `gchat` turns on Google Chat (see
   "Google Chat" below).
7. Write a request in the General topic.

Steps 3, 5 and 6 can also be done by asking the orchestrator in General (for
example "create project web and bind the Jira folder to it"). It proposes each
change in `fleet · needs you` and applies it only when you tap Approve. Bot
tokens, secret values and node tokens are still entered on the web pages: the
orchestrator sends you a login link to the right one.

### Slack and the secretary

The secretary reads **every** conversation you are in (DMs, group DMs, private
and public channels), in real time, through a Slack app in Socket Mode. With
only `SLACK_USER_TOKEN` set, the fleet still polls the channels mapped to
projects, as before, but the secretary sees nothing else.

In your Slack app (api.slack.com/apps → the app):

1. **Socket Mode** → turn on **Enable Socket Mode**. Create an app-level
   token with the `connections:write` scope: turning Socket Mode on may ask for
   one, or use **Basic Information** → **App-Level Tokens** → **Generate Token
   and Scopes**. That `xapp-...` token is `SLACK_APP_TOKEN`.
2. **OAuth & Permissions** → **User Token Scopes**: `channels:history`,
   `channels:read`, `groups:history`, `groups:read`, `im:history`, `im:read`,
   `mpim:history`, `mpim:read`, `users:read`, and `chat:write` (only for
   replies you approve).
3. **Event Subscriptions** → **Enable Events** → **Subscribe to events on
   behalf of users** (Slack's docs call these "Workspace Events"):
   `message.channels`, `message.groups`, `message.im`, `message.mpim`. Save.
   With Socket Mode on, no request URL is needed.
4. **Install** (or **Reinstall**) **to Workspace**, then copy the **User OAuth
   Token** (`xoxp-...`): that is `SLACK_USER_TOKEN`. Adding scopes later means
   reinstalling and copying the new token.
5. Fleet web UI → **Secrets** → folder `slack` → set both keys.

Slack's settings pages change now and then; if a name above differs, look for
the same setting nearby.

- **Keep the app internal** (do not distribute it). Internal apps keep the
  Tier 3 limits on `conversations.history`/`replies` (about 50 calls a minute)
  that catching up after an outage relies on; since 2025-05-29, apps that are
  distributed but not in the Marketplace get 1 call a minute and 15 messages a
  page.
- **Run one orchestrator per `SLACK_APP_TOKEN`.** Slack spreads events across
  all of an app's open connections, so a second process (a staging
  orchestrator, say) silently takes a share of the messages. The orchestrator
  logs a warning when Slack reports more than one connection.
- If the tokens stop working (revoked, wrong type), the orchestrator says so in
  General and stops the feed until the `slack` folder changes.

The secretary runs as its own session in `FLEET_BRAIN_PROJECT` (so on the
brain's node), on `FLEET_SECRETARY_MODEL` (Haiku by default). Its todos, rule
changes and answers appear in the **📋 Inbox** topic, created on first use;
write there to talk to it ("what did I miss since yesterday?", "ignore
#random"). The web UI's **Inbox** page lists open todos, rules and recent
triage. It gets no secrets, runs in an empty directory of its own, and may run
only `fleet agent` commands. The node it runs on must not add permission-widening
flags to Claude's command (`--dangerously-skip-permissions`, `--add-dir`,
`--permission-mode`, `--allowedTools`, `--settings`, `--tools`,
`--mcp-config`, `--plugin-dir`): the secretary is refused on such a node. Its settings
(digest hour, time zone, batching, retention) are in `deploy/.env.example`.

Every message the secretary cites (in a todo, an escalation note, a digest or
a reply draft) is labelled with where it came from, e.g. "Slack ·
#gitlab-notifications · Serdar" or "Google Chat · Project X · Olya", the same
way in Telegram and on `/inbox`.

### Replying from Telegram

Reply in the Inbox to one of the secretary's posts that cites a source
message (a todo, a note) with what you want said ("tell him Friday works").
The secretary drafts the actual message and posts a card with **Send**,
**Edit** and **Cancel**: nothing reaches Slack or Google Chat until you tap
Send. Reply to the card itself with new wording to replace the draft before
sending, word for word.

### Google Chat

Google Chat has no equivalent of Slack's Socket Mode, so this is a polling
feed (about every 30 seconds): the secretary sees new messages in every space
you are a member of, direct messages included, from the moment it starts (no
history import). Muted/notification settings are not exposed by this API, so
use an owner rule in the Inbox instead if you want a space treated as low
priority.

You need an OAuth **"authorized user" credential** (not a service account)
with these scopes: `chat.spaces.readonly`, `chat.messages.readonly` and
`chat.messages.create` (the last one only if you want to send replies). How
you obtain one is up to you (a Google Cloud OAuth client plus a one-time
consent flow, e.g. with `google-auth-oauthlib`, or any tool that writes the
standard "authorized_user" JSON). Fleet web UI → **Secrets** → folder
`gchat` → set:

- `GOOGLE_CLIENT_ID`
- `GOOGLE_CLIENT_SECRET`
- `GOOGLE_REFRESH_TOKEN`

If the refresh token is revoked, or its scopes no longer cover what the
secretary needs, the orchestrator says so in General and the feed stays off
until the `gchat` folder changes.

### The daily retrospective

Once a day the fleet reviews its own day — tasks, your decisions and requests,
the agents' chatter, and what went wrong — and proposes improvements **to the
fleet itself** in the **📈 Retro** topic. One tap on a proposal turns it into a
task on the fleet's own code; `/retro` lists them, and the web UI has a Retro
page.

It is off until you give it an hour, and a proposal cannot become a task until
it has somewhere to work:

1. **A clone of this repository**, separate from any working tree you edit
   yourself, on a node that can actually build it — it needs `cargo`, `node`
   and (for the Postgres-backed e2e) a Postgres it may create databases in. A
   VPS with only `git` cannot verify a Rust change, so prefer a PC node:

   ```sh
   git clone https://github.com/<you>/agent-management.git C:\repos\fleet-src
   ```

2. **A project for it**, created like any other on the Projects page (slug
   `fleet-src` by default, `FLEET_RETRO_PROJECT` to change it), then mapped to
   that clone on the node that holds it:

   ```sh
   fleet project map fleet-src C:\repos\fleet-src
   ```

   Give it no secret folder: with no credentials the agent cannot push, so its
   work stays a local branch until you look at it. That is deliberate — the
   brief it gets says to branch, keep `fmt`, `clippy` and the tests green, and
   never push or deploy.

3. **The hour**, in `.env` (`FLEET_TIMEZONE` decides the zone), plus the model
   it should think with:

   ```sh
   FLEET_RETRO_HOUR=7
   FLEET_RETRO_MODEL=opus
   FLEET_RETRO_EFFORT=high
   ```

The retrospective holds no secrets, runs in an empty directory with only
`fleet agent` in its shell, and can do exactly two things: propose and
conclude. It cannot create a task, answer you, message an agent or change a
setting — every proposal waits for your tap.

## 7. Backups

`deploy/scripts/backup.sh` dumps both databases and the roles, and archives
the orchestrator's data volume (which holds `node.key`, its iroh identity). It
keeps 14 days by default. Run it nightly from cron on the VPS:

```sh
sudo touch /var/log/fleet-backup.log && sudo chown "$USER" /var/log/fleet-backup.log
crontab -e
# 17 3 * * * /opt/fleet/deploy/scripts/backup.sh >> /var/log/fleet-backup.log 2>&1
```

Run it once by hand and check `deploy/backups/<timestamp>/`. Then copy that
folder off the VPS regularly (restic, rclone, rsync to another machine): a
backup on the same disk does not survive losing the VPS. `.env` is not in the
backup; keep it (especially `FLEET_MASTER_KEY`) in your password manager.

### Restore

On a fresh VPS, set up `.env` with the **same** values, then:

```sh
cd /opt/fleet/deploy
docker compose up -d postgres          # first start creates roles, databases, extensions
B=backups/<timestamp>
docker compose exec -T postgres pg_restore -U postgres -d fleet     --clean --if-exists < $B/fleet.dump
docker compose exec -T postgres pg_restore -U postgres -d hindsight --clean --if-exists < $B/hindsight.dump
docker compose build orchestrator
docker compose run --rm --no-deps -T --entrypoint tar orchestrator -C /var/lib/fleet -xzf - < $B/fleet-data.tar.gz
docker compose up -d
```

`globals.sql` is only needed if you restore into a server where the init script
did not run (it recreates the `fleet` and `hindsight` roles).

## 8. Updating

### Automatically: a merge to main is a deploy

**Merging into `main` deploys.** When CI passes on `main`, its `Tag and release` job tags
that exact commit with the next patch version (`v0.2.0`, `v0.2.1`, …) and dispatches
`release.yml`, which publishes the binaries with SHA-256 sums. On the VPS,
`fleet-update.timer` notices the new release within five minutes and rolls it out:

1. checks the tag is **on `main`** — a tag pushed anywhere else is refused, so the owner's
   merge stays the only way in — then checks it out in `/opt/fleet` and builds its image
   (`FLEET_VERSION=<tag>`);
2. **dry-runs its migrations against the live database** (`fleet migrate --dry-run`: all
   pending migrations in one transaction, then rolled back) and stops if any fails —
   CI's empty database cannot show what a migration does to rows that already exist. It
   also refuses what a real start would (a half-applied or edited migration). It waits at
   most 5 s for a lock, so it never stalls the live orchestrator; a dry run that ran out of
   time waiting is retried next tick rather than counted as a failure;
3. backs the database up to `/root/backups/fleet-before-<tag>.sql.gz` (the newest ten are
   kept) and checks the dump is real;
4. swaps the image in and waits for 20 s of continuous health (running, the right version,
   the web answering), **rolling back to the previous image** if it does not get there —
   and health-checking that too, so the message says whether the rollback really worked.
   Rolling back is possible because the migrator ignores migrations it does not know and new
   columns are nullable or defaulted (CLAUDE.md);
5. updates this host's node once no agent is running on it, or after two hours regardless
   (runs resume from their sessions). A node binary that will not start is put back and
   not tried again until the next release.

After any failure `/opt/fleet` is put back on the commit that is running, so a hand deploy
cannot build the release the updater rejected. It says what it did in the Telegram group —
quietly on success, loudly on failure. A release that failed is not retried; the next one
is. To retry it anyway: `rm /var/lib/fleet-update/failed-<tag>`.
`journalctl -u fleet-update` has the detail, and `fleet --version` on any binary says which
release it is.

**The updater runs as root, so it is the one part of a release that never updates itself.**
systemd runs the installed copy, `/usr/local/sbin/fleet-update`; when a release changes
`deploy/update/fleet-update.sh`, the group is told once, and a person applies it after
reading it. Install it once:

```sh
install -m 0755 /opt/fleet/deploy/update/fleet-update.sh /usr/local/sbin/fleet-update
cp /opt/fleet/deploy/update/fleet-update.{service,timer} /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now fleet-update.timer
systemctl start fleet-update        # one run now; then `journalctl -u fleet-update`
```

### By hand

**The stack** — from `main`. Deploy only what CI called green, and prefer letting the
updater do it: by hand you skip the migration dry run, which is the step that would have
caught the 2026-11-20 outage.

```sh
cd /opt/fleet && git fetch origin && git merge --ff-only origin/main
cd deploy
docker compose build orchestrator      # or set FLEET_IMAGE=ghcr.io/<owner>/fleet:<version> and: docker compose pull orchestrator
docker compose pull postgres hindsight iroh-relay caddy
docker compose up -d
docker image prune -f
```

- Hindsight and iroh-relay are pinned (`HINDSIGHT_VERSION`,
  `IROH_RELAY_VERSION` in `.env`). Raise them deliberately: keep the relay in
  step with the `iroh` version in `Cargo.lock`, and read Hindsight's release
  notes (it migrates its database on start).
- `pgvector/pgvector:pg16` stays on Postgres 16. A major Postgres upgrade
  needs a dump and restore (section 7), not just a new tag.
- Take a backup first (`deploy/scripts/backup.sh`).

**Nodes:** the VPS's own node follows automatically (above). Other nodes do not yet —
pushing releases to them over the link is the next step of auto-deploy. Until then, by
hand — and **update the nodes whenever the
`fleet agent` CLI changed**, not only on a protocol bump: that CLI lives in the
node's binary, so a new agent command (or a changed one) simply does not exist
for the sessions on a node still running the old one. The orchestrator will
offer the tool, the agent will call it, and it will get "unrecognised
subcommand".

- Linux: replace `/usr/local/bin/fleet`, then `sudo systemctl restart fleet-node`.
  On the VPS the binary comes out of the image that was just built:

  ```sh
  CID=$(docker create fleet:local)
  docker cp "$CID:/usr/local/bin/fleet" /usr/local/bin/fleet.new
  docker rm -v "$CID"
  mv /usr/local/bin/fleet.new /usr/local/bin/fleet && chmod 755 /usr/local/bin/fleet
  sudo systemctl restart fleet-node
  su - fleet -c 'fleet agent --help'   # check it is the new one
  ```
- Windows: rerun `install-node.ps1 -FleetExe <new exe> -TrayExe <new tray exe>`
  (no `-Ticket`). It stops the node and the tray, replaces the binaries and
  restarts the task; the node's key is kept.
- A release that raises the protocol version (`PROTOCOL_VERSION` in
  `crates/proto`) refuses nodes on the old version with "protocol version N is
  not supported": update the orchestrator and every node together. Version 3
  added the project list for the tray app; version 4 added the nodes' disk
  reports (free space under `~/.claude` and the project directories).

## Troubleshooting

- **Relay keeps restarting:** `docker compose logs iroh-relay`. "failed to
  read" the certificate means Caddy has not got it yet: check
  `docker compose logs caddy` for ACME errors (DNS not pointing here, port 80
  or 443 blocked). The relay expects the certificate from Let's Encrypt; the
  `RELAY_DOMAIN` block in the `Caddyfile` pins that issuer.
- **Nodes only connect through the relay:** check that 7700/udp and 7842/udp
  are open in every firewall in front of the VPS, and that `FLEET_PUBLIC_ADDR`
  is the VPS's public IP. If direct connections still fail, run the
  orchestrator with `network_mode: host` (and drop its `ports:`), which removes
  Docker's NAT from the UDP path.
- **Hindsight unhealthy:** `docker compose logs hindsight`. The first start
  can take several minutes; an Anthropic key error shows up on the first
  retain, not at start.
- **Bot gets no messages:** privacy mode still on (BotFather `/setprivacy`), or
  the bot is not an admin. After changing privacy mode, remove the bot from
  the group and add it again.
