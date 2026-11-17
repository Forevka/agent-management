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
   *Manage Topics*, *Delete Messages* and *Pin Messages*.
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
   their tokens are entered in the web UI later, not in `.env`.

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

Every node needs a one-time ticket from the orchestrator:

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


### After the first node: projects, bots, Claude

1. In Telegram, send `/web` to get a login link for the web UI.
2. **Agents** page: register the agent bots' tokens (each bot must already be a
   member of the group).
3. **Projects** page: create each project (its slug is what nodes map), bind
   secret folders, add Slack channels.
4. On each host, map the project: `fleet project map <slug> <directory>`.
5. **Nodes** page: per node, set the Claude command (e.g. a full path to
   `claude.exe`) and authentication (the host's own login, a
   `claude setup-token` token, or an API key).
6. **Secrets** page: a folder named `slack` with `SLACK_USER_TOKEN` turns on
   the Slack feed.
7. Write a request in the General topic.

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

**The stack:**

```sh
cd /opt/fleet && git pull
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

**Nodes:** releases are pushed to nodes once update-on-approval is
implemented. Until then, by hand:

- Linux: replace `/usr/local/bin/fleet`, then `sudo systemctl restart fleet-node`.
- Windows: rerun `install-node.ps1 -FleetExe <new exe>` (no `-Ticket`). It
  stops the node, replaces the binary and restarts the task; the node's key is
  kept.

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
