# Setting up a new fleet node — instructions for the Claude Code session doing it

You are running on a machine that is about to become a **node** of the owner's agent fleet: the
fleet's orchestrator will start Claude Code sessions here, in project folders, to do work. Your
job is to install the node, join it to the fleet, and map the projects it should work on. The
person you are talking to is **the owner** of the fleet.

Follow this file in order. It is written so you never have to guess: wherever something only the
owner knows or decides is needed, it says **ASK THE OWNER**, and you stop and ask. Do not make up
a value, skip a check, or continue past a failed step. If something here does not match what you
see, say so and ask rather than improvising.

The longer, human-oriented version of all this is `deploy/README.md` §6 in the same repository.

## Ground rules

- **Never invent a value.** The invite ticket, project folders, repository URLs and which projects
  to map all come from the owner. Ask, in one short message, and wait.
- **The invite ticket is a credential.** It joins one machine to the fleet, once. Use it only in the
  install command. Do not write it to a file, print it back, or put it in your final report.
- **Do not run anything elevated on Windows** (no "Run as administrator"). The node runs as the
  logged-in user, because it runs Claude Code with that user's login. On Linux, `sudo` is needed
  only for the steps marked so — ask before using it.
- **Stop on any checksum mismatch.** A binary that does not match its published SHA-256 is not
  installed, whatever the reason.
- **If the fleet is already installed here, ask before touching it** (step 2 checks). An update and
  a fresh join are different operations, and a second join with a new ticket replaces this
  machine's identity.
- **Do not add MCP servers, plugins or secrets** to this machine or to any project folder as part of
  this setup. The fleet gives its agents what they need.
- Leave nothing half-done: either finish, or say exactly which step failed and what it printed.

Fixed facts you can rely on (not secrets):

| | |
|---|---|
| Repository | `https://github.com/Forevka/agent-management` (public) |
| Releases | `https://github.com/Forevka/agent-management/releases` |
| The fleet's relay | `https://relay.agents.forevka.dev` |

## 1. Which kind of machine is this?

Find the OS and architecture.

- **Windows x64** → continue with the Windows steps below. Use **PowerShell** for every command.
- **Linux x86_64** → skip to [Linux](#linux).
- **macOS, or anything on ARM** → **stop.** The fleet publishes builds only for Windows x64 and
  Linux x86_64. Tell the owner that this machine needs a build target added to `release.yml`
  first, and do nothing else.

## 2. Is Claude Code ready, and is the fleet already here?

1. Run `claude -p "say hi"`. It must print an answer.
   - If `claude` is not found or asks to log in: **ASK THE OWNER** to install Claude Code and sign
     in as this user (`claude`, then `/login`), then run the check again. The node cannot work
     without it.
2. Check for an existing install:
   - Windows: does `$env:LOCALAPPDATA\Programs\fleet\fleet.exe` exist? Is there a scheduled task
     named `fleet node` (`Get-ScheduledTask -TaskName 'fleet node' -ErrorAction SilentlyContinue`)?
   - Linux: does `/usr/local/bin/fleet` exist, or a `fleet-node` systemd service?

   If either exists: **ASK THE OWNER** whether this is an *update* of an existing node (then no
   ticket is needed — see [Updating](#updating-an-existing-node)) or a *fresh* setup.

## 3. Windows: download and verify

Find the newest release, then download into a new, empty folder:

```powershell
$tag = (Invoke-RestMethod https://api.github.com/repos/Forevka/agent-management/releases/latest).tag_name
$v   = $tag.TrimStart('v')
"newest release: $tag"
$base = "https://github.com/Forevka/agent-management/releases/download/$tag"
$dir  = Join-Path $HOME 'fleet-setup'
New-Item -ItemType Directory -Force $dir | Out-Null
Set-Location $dir
foreach ($f in "fleet-$v-x86_64-pc-windows-msvc.exe", "fleet-tray-$v-x86_64-pc-windows-msvc.exe", 'SHA256SUMS') {
    Invoke-WebRequest "$base/$f" -OutFile $f -UseBasicParsing
}
Invoke-WebRequest "https://raw.githubusercontent.com/Forevka/agent-management/$tag/deploy/windows/install-node.ps1" `
    -OutFile install-node.ps1 -UseBasicParsing
```

The installer is taken **from the same tag** as the binaries, so the two always match.

Verify every binary against the release's sums. Each line must say `OK`:

```powershell
Get-Content SHA256SUMS | ForEach-Object {
    $h, $n = $_ -split '\s+', 2; $n = $n.TrimStart('*')
    if (Test-Path $n) { if ((Get-FileHash $n).Hash.ToLower() -eq $h) { "OK   $n" } else { "BAD  $n" } }
}
```

Any `BAD`: **stop**, tell the owner which file, and install nothing.

Check the binary says what it is: `.\fleet-$v-x86_64-pc-windows-msvc.exe --version` should print
`fleet v<version>`.

## 4. Windows: the ticket, then install

**ASK THE OWNER** for an invite ticket, with exactly this:

> To join this machine I need a one-time invite ticket. Please send **`/invite`** in the fleet's
> Telegram group; it answers with `fleet join <ticket>`. Paste me the ticket (the part after
> `fleet join`).

When you have it, install and join — in a normal, not elevated, PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-node.ps1 `
    -FleetExe ".\fleet-$v-x86_64-pc-windows-msvc.exe" `
    -TrayExe  ".\fleet-tray-$v-x86_64-pc-windows-msvc.exe" `
    -Relay https://relay.agents.forevka.dev `
    -Ticket '<the ticket>'
```

It copies `fleet.exe` and `fleet-tray.exe` to `%LOCALAPPDATA%\Programs\fleet`, joins, and
registers the scheduled task **fleet node**, which starts at logon and restarts the node whenever
it exits.

If it says the ticket is invalid or already used: tickets are single-use. **ASK THE OWNER** for a
new one (`/invite` again). Never retry the same ticket.

> **Important for every `fleet` command after this point.** The installer sets `FLEET_DATA_DIR`
> and adds the install folder to `PATH` as *user* environment variables. Those reach only shells
> started **after** the install — not yours, whose environment was fixed when you started. So
> from here on, always call the binary by its full path and pass the data directory explicitly:
>
> ```powershell
> $fleet = "$env:LOCALAPPDATA\Programs\fleet\fleet.exe"
> $data  = "$env:LOCALAPPDATA\fleet\data"
> ```
>
> Without `--data-dir $data`, `fleet` falls back to `%LOCALAPPDATA%\fleet` — a different folder —
> and a project mapping written there is silently ignored by the node.

## 5. Windows: check it is alive

```powershell
& $fleet --version
(Get-ScheduledTask -TaskName 'fleet node').State          # Running
Get-Content "$env:LOCALAPPDATA\fleet\logs\node.log" -Tail 20
Get-Content "$data\status.json" | ConvertFrom-Json | Select-Object node_name, connected, projects
```

`connected` must be `True` within a minute. `projects` lists the fleet's projects — the ones this
node can be given. If it does not connect within two minutes, show the owner the last 40 lines of
`node.log` and stop.

Continue with [Projects](#6-projects).

## Linux

Use a shell as the user who is logged into Claude Code (call it `$U`). Steps marked **sudo** need
it: ask the owner before the first one.

```sh
tag=$(curl -fsSL https://api.github.com/repos/Forevka/agent-management/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
v=${tag#v}; echo "newest release: $tag"
base=https://github.com/Forevka/agent-management/releases/download/$tag
raw=https://raw.githubusercontent.com/Forevka/agent-management/$tag
mkdir -p ~/fleet-setup && cd ~/fleet-setup
curl -fsSLO "$base/fleet-$v-x86_64-unknown-linux-gnu"
curl -fsSLO "$base/SHA256SUMS"
curl -fsSL "$raw/deploy/systemd/fleet-node.service" -o fleet-node.service
curl -fsSL "$raw/deploy/systemd/node.env.example"   -o node.env.example
grep -- "fleet-$v-x86_64-unknown-linux-gnu\$" SHA256SUMS | sha256sum -c -   # must print OK
```

Not `OK`: **stop** and install nothing. Then:

```sh
sudo install -m 0755 "fleet-$v-x86_64-unknown-linux-gnu" /usr/local/bin/fleet     # sudo
fleet --version                                                                      # fleet v<version>
```

**ASK THE OWNER** for the ticket, worded as in step 4. Join; `fleet join` keeps running as the node
once joined, so stop it with Ctrl+C when it logs `connected to orchestrator`:

```sh
FLEET_DATA_DIR=$HOME/.local/share/fleet FLEET_RELAY=https://relay.agents.forevka.dev \
  fleet join '<the ticket>'
```

Then the service (all **sudo**), with `U` set to this user's name:

```sh
U=$(whoami)
sudo install -d -m 0755 /etc/fleet
sed "s/CHANGE_ME/$U/g" node.env.example | sudo tee /etc/fleet/node.env >/dev/null
sudo chown root:"$U" /etc/fleet/node.env && sudo chmod 0640 /etc/fleet/node.env
sed "s/CHANGE_ME/$U/g" fleet-node.service | sudo tee /etc/systemd/system/fleet-node.service >/dev/null
sudo systemctl daemon-reload && sudo systemctl enable --now fleet-node
journalctl -u fleet-node -n 30 --no-pager
```

Open `/etc/fleet/node.env` and check `FLEET_RELAY` is `https://relay.agents.forevka.dev` and that
`PATH` includes the folder `claude` is in (`command -v claude`). Fix them if not, then
`sudo systemctl restart fleet-node`. On Linux, use
`fleet --data-dir "$HOME/.local/share/fleet" ...` in the steps below.

## 6. Projects

The node only works on projects mapped to a folder here. Show the owner the projects listed in
`status.json` (step 5), then **ASK THE OWNER**, in one message:

> Which of these projects should this machine work on? For each one: is the repository already
> cloned here (and where), or should I clone it — and from which URL into which folder?

For each project the owner picks:

1. If it needs cloning, clone it where the owner said. A private repository needs the owner's own
   git access on this machine; if the clone asks for credentials, **ASK THE OWNER** to sign in
   (e.g. `gh auth login`, or their Git credential manager) rather than handling a password or
   token yourself.
2. Map it:

   ```powershell
   & $fleet --data-dir $data project map <project-slug> '<folder>'
   ```

3. Check: `& $fleet --data-dir $data project list` shows it, with the folder you mapped.

The tray icon (Windows) does the same with a click; prefer the command, since you can check it.

## 7. A laptop should stay awake to work

The node only works while this user is logged in and the machine is awake: a locked screen is
fine, sleep is not. Work queues safely while it sleeps, so nothing is lost — it just waits. Tell
the owner, and **ASK THE OWNER** whether to change the power settings to not sleep while plugged
in. Do not change them without a yes.

## 8. Report

Finish with a short report for the owner — and no ticket in it:

- the node's name (`status.json` → `node_name`) and `fleet --version`;
- `connected: true`, and how the node starts (scheduled task / systemd service);
- the projects mapped, and their folders;
- anything left for the owner. Usually: on the web UI's **Nodes** page
  (`https://agents.forevka.dev/nodes`), check this node's **max agents** (how many sessions it may
  run at once) suits this machine — the default is 4, a lot for a laptop.

## Updating an existing node

Windows nodes do not update themselves yet. To move one to the newest release: steps 3 and 5 as
above, and in step 4 run the installer **without `-Ticket`** — it stops the node and the tray,
replaces the binaries and starts the task again, keeping the node's identity. Linux: replace
`/usr/local/bin/fleet` (**sudo**) and `sudo systemctl restart fleet-node`.

## When something goes wrong

| What you see | What it means |
|---|---|
| `protocol version N is not supported` | The binary is older or newer than the orchestrator. Use the newest release; if it still says so, tell the owner — the orchestrator itself may need updating. |
| `ticket` invalid / already used | Single-use. Ask the owner for a new `/invite`. |
| `connected: false` for minutes | Show the owner the end of the node log. Often a firewall blocking outbound UDP, or the wrong relay. |
| Agents start but fail at once | `claude -p "say hi"` as this user — Claude Code may be logged out. On Linux, `PATH` in `/etc/fleet/node.env` must reach `claude`. |
| A mapping "does nothing" | It was written to the wrong data directory: always pass `--data-dir` (see step 4). |

Logs: Windows `%LOCALAPPDATA%\fleet\logs\node.log`; Linux `journalctl -u fleet-node`.
