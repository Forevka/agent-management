# fleet-tray

A tray icon and a small window for managing a node's local projects: which fleet project slug
maps to which folder on this machine. It does what `fleet project map`, `unmap` and `list` do,
on the same `node.json`, using the same rules (it uses `crates/node-config`, shared with the
node).

- Left click on the tray icon shows or hides the window, a double click shows it; the menu has
  **Open** and **Quit tray**. Closing the window only hides it.
- The header shows the node's status from `status.json` in the data directory. The node writes
  it on start, on every connect and disconnect, and whenever the orchestrator sends its project
  list (on connect and after every project create or edit). Without the file it says "status
  unknown"; if the pid in it is no longer running, it says "node not running" and its project
  list is not used.
- When `status.json` lists the orchestrator's projects, the slug is picked from a dropdown and
  mappings the orchestrator does not know are flagged. Without that list, type the slug.
- The node re-reads `node.json` every 5 seconds, so new mappings apply without a restart.

## How it is launched

```
fleet-tray.exe [--data-dir <path>] [--node-pid <pid>]
```

- `--data-dir` defaults to `FLEET_DATA_DIR`, then to the `fleet` CLI's default
  (`%LOCALAPPDATA%\fleet` on Windows, `~/.local/share/fleet` elsewhere).
- On Windows, `fleet node` starts it once at startup, detached, with
  `--data-dir <its data dir> --node-pid <its pid>`, when `fleet-tray.exe` sits next to
  `fleet.exe` and `FLEET_NO_TRAY` is not set. The installer (`deploy/windows/install-node.ps1
  -TrayExe`) puts it there. A tray the owner quit is not started again until the node restarts.
  `FLEET_NO_TRAY` is read when the node starts, so changing it takes a node restart. `fleet
  join` never starts the tray.
- With `--node-pid` the tray starts hidden and exits once that process has stopped, unless
  `status.json` by then names a different, running node (a restart): it then watches that one.
  Without `--node-pid`, the window opens at once and the tray runs until **Quit tray**.
- A pid can be reused once its process exits, so the tray checks process identity: on Windows
  it holds a handle to the node process; elsewhere it compares the pid's start time with the one
  taken when it started watching. `status.json` carries the node's start time too, and a status
  whose pid now belongs to another process counts as stale ("node not running") and is never
  followed.
- Only one tray runs per user, and it serves one data directory at a time. A later launch is
  handed to the running tray:
  - with `--node-pid` (a node, for example one that restarted): the tray switches to that
    launch's data directory and watches the new pid, without opening the window;
  - without it (by hand): the window opens, and the tray switches to that launch's data
    directory unless it is currently serving a node, which takes precedence.

  Only the arguments and working directory reach the running tray, not the environment, so a
  later launch that relies on `FLEET_DATA_DIR` is resolved with the running tray's environment.
  Pass `--data-dir` to be explicit.
- Only the `projects` key of `node.json` is changed; every other key, including the node's
  settings, is written back as it was. The edit holds `node.json.lock`, the lock the node and
  `fleet project map` take for theirs.
- Problems that cannot be shown in the window (a bad command line, a node pid that cannot be
  checked, a data-directory switch) go to `<data-dir>/logs/tray.log`.

## Build

It is its own Cargo workspace (it is not part of the root `crates/*` workspace) and depends on
`crates/node-config` by path, which keeps the node's network stack out of the build. Needs
Node 22+ and the Rust toolchain. CI builds and checks it on Windows, and the release workflow
ships it as `fleet-tray-<version>-x86_64-pc-windows-msvc.exe`.

```
cd apps/tray
npm ci
npm run tauri build -- --no-bundle   # -> src-tauri/target/release/fleet-tray.exe
```

Checks:

```
npm run check                                    # svelte-check
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`npm run tauri dev` runs it against the Vite dev server. The icons in `src-tauri/icons` come from
`app-icon.svg` via `npx tauri icon app-icon.svg -o src-tauri/icons` (the mobile and Store sizes
are removed afterwards).
