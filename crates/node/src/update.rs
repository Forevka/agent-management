//! A node replacing its own binary with a newer release.
//!
//! The orchestrator says which release to move to ([`fleet_proto::UpdateAvailable`]): on connect
//! when this node is behind, or — across a protocol bump, when it cannot talk to this node at all
//! — inside the refusal's text ([`fleet_proto::parse_update_hint`]). Either way the node
//! (`Node::start_update`, one update at a time, retried with backoff until it lands or a newer
//! release replaces it):
//!
//! 1. downloads the release's binary for this platform and the release's `SHA256SUMS` — only from
//!    this build's own GitHub repository ([`fleet_proto::release_base_url`]), over https only —
//!    and refuses it unless the hash matches;
//! 2. runs the download with `--version` and refuses it unless it says it is that release;
//! 3. waits until no agent is running here: [`DEADLINE`] at most, or [`OWNER_DEADLINE`] while a
//!    run is waiting on the owner (a plan or a question), so nobody's pending approval is thrown
//!    away by an update;
//! 4. renames its own running executable aside — Windows allows renaming a running exe, not
//!    replacing it — puts the new one in its place, and exits. The supervisor starts the new one.
//!
//! **Only under a supervisor that asked for it** ([`SELF_UPDATE_ENV`], set by the Windows
//! launcher), since a binary that exits needs something to start the next one, and to put the
//! previous one back when the next one is bad. The VPS node (root-owned binary, updated by its
//! host), `fleet join`, and a developer's `cargo run` never replace themselves.
//!
//! The previous binary is kept as `fleet.old.exe` until the new one has connected and stayed up
//! ([`watch_new_binary`]). The launcher puts it back if the new one keeps dying at startup —
//! recording that release in `fleet.rejected`, so the node does not take it again — or exits with
//! [`EXIT_ROLL_BACK`] because it never connected (not recorded: the machine may only have been
//! offline, and the release is tried again later).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use fleet_proto::UpdateAvailable;

/// How long a busy node may put an update off before stopping its runs to apply it.
pub const DEADLINE: Duration = Duration::from_secs(2 * 3600);
/// The same, while some run is waiting on the owner (its tool call is open).
pub const OWNER_DEADLINE: Duration = Duration::from_secs(24 * 3600);
/// After a failed attempt, the first wait before trying again; it doubles up to [`RETRY_MAX`].
/// A release's binaries are published some minutes after its tag, and the orchestrator may offer
/// it before they exist.
pub const RETRY_MIN: Duration = Duration::from_secs(5 * 60);
pub const RETRY_MAX: Duration = Duration::from_secs(2 * 3600);

/// The variable a supervisor sets to `1` to let the node replace its own binary.
pub const SELF_UPDATE_ENV: &str = "FLEET_SELF_UPDATE";
/// How the node tells the launcher "this binary never connected; put the previous one back".
pub const EXIT_ROLL_BACK: i32 = 87;
/// How long a freshly updated binary has to connect before it asks to be rolled back.
pub const CONNECT_WITHIN: Duration = Duration::from_secs(30 * 60);
/// How long it must then stay up before the previous binary is deleted.
pub const PROVEN_AFTER: Duration = Duration::from_secs(10 * 60);

/// Whether this node may replace its own binary: its supervisor asked for it, and the directory
/// the binary lives in is writable.
pub fn enabled(exe: &Path) -> bool {
    std::env::var(SELF_UPDATE_ENV).is_ok_and(|v| v == "1") && can_replace(exe)
}

/// Whether the directory `exe` lives in is writable. The VPS node's lives in `/usr/local/bin`,
/// owned by root.
pub fn can_replace(exe: &Path) -> bool {
    let probe = exe.with_file_name(".fleet-update-probe");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The release asset that is this platform's `fleet` binary, e.g.
/// `fleet-0.2.7-x86_64-pc-windows-msvc.exe` for `v0.2.7`. `None` for a platform no release builds.
pub fn asset_name(version: &str) -> Option<String> {
    let v = version.strip_prefix('v')?;
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc.exe",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => return None,
    };
    Some(format!("fleet-{v}-{target}"))
}

const EXE_SUFFIX: &str = if cfg!(windows) { ".exe" } else { "" };

/// Where the previous binary is kept, next to the running one.
pub fn previous_path(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("fleet.old{EXE_SUFFIX}"))
}

/// Where a downloaded release waits to be swapped in: one file per version, so a second update
/// can never write over the file a first one is about to rename into place.
fn staged_path(exe: &Path, version: &str) -> PathBuf {
    exe.with_file_name(format!("fleet-{version}.new{EXE_SUFFIX}"))
}

/// Where the launcher records a release it rolled back (`fleet <version>`, as `--version` says).
fn rejected_path(exe: &Path) -> PathBuf {
    exe.with_file_name("fleet.rejected")
}

/// Whether the launcher rolled `version` back on this machine. Such a release is not taken again;
/// the next one is.
pub fn rejected(exe: &Path, version: &str) -> bool {
    std::fs::read_to_string(rejected_path(exe))
        .is_ok_and(|s| s.trim() == format!("fleet {version}"))
}

/// The SHA-256 a `SHA256SUMS` file gives for `name` (the `sha256sum` format: `<hex>  <name>`,
/// with an optional `*` before binary-mode names).
pub fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(bytes))
}

/// Downloads the release, checks it, and leaves it next to `exe`, ready to swap in. Returns its
/// path. Nothing about the running binary changes here.
pub async fn prepare(u: &UpdateAvailable, exe: &Path) -> Result<PathBuf> {
    // The orchestrator says which release, never where from: a node only takes binaries from the
    // repository its own binary was built for.
    let base = fleet_proto::release_base_url(&u.version);
    ensure!(
        u.base_url.trim_end_matches('/') == base,
        "refusing to update from {}: this build only takes releases from {base}",
        u.base_url
    );
    let name = asset_name(&u.version)
        .with_context(|| format!("no {} build exists for this platform", u.version))?;
    // reqwest is built without a TLS provider of its own; ring is installed process-wide, as
    // every other HTTP client in the fleet does. Without it the first https request fails.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(30))
        // Per read, not for the whole body: a slow link may take minutes for the binary, but a
        // stalled one should not hold the update for ever.
        .read_timeout(Duration::from_secs(120))
        .build()?;
    let get = |url: String| {
        let http = http.clone();
        async move {
            let resp = http.get(&url).send().await?.error_for_status()?;
            anyhow::Ok(resp.bytes().await?)
        }
    };

    let sums = get(format!("{base}/SHA256SUMS")).await.context(
        "fetching the release's SHA256SUMS (a release's files appear some minutes after its tag)",
    )?;
    let want = expected_hash(&String::from_utf8_lossy(&sums), &name)
        .with_context(|| format!("SHA256SUMS has no line for {name}"))?;
    let bytes = get(format!("{base}/{name}"))
        .await
        .with_context(|| format!("downloading {name}"))?;
    let got = sha256_hex(&bytes);
    ensure!(
        got == want,
        "{name} does not match its published SHA-256 (got {got}, expected {want}); not installing it"
    );

    // Written aside and renamed, so the staged path only ever holds a whole binary.
    let staged = staged_path(exe, &u.version);
    let partial = staged.with_extension("partial");
    std::fs::write(&partial, &bytes).with_context(|| format!("writing {}", partial.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&partial, &staged)
        .with_context(|| format!("moving the download to {}", staged.display()))?;

    // The hash says the bytes are the published ones; this says the published ones are the
    // release we were told about, and that the binary runs at all on this machine.
    let run = tokio::process::Command::new(&staged)
        .arg("--version")
        .kill_on_drop(true)
        .output();
    let says = match tokio::time::timeout(Duration::from_secs(30), run).await {
        Ok(out) => out.with_context(|| format!("running {} --version", staged.display()))?,
        Err(_) => {
            let _ = std::fs::remove_file(&staged);
            bail!("{} --version did not answer within 30 s", staged.display());
        }
    };
    let says = String::from_utf8_lossy(&says.stdout).trim().to_owned();
    if says != format!("fleet {}", u.version) {
        let _ = std::fs::remove_file(&staged);
        bail!(
            "the downloaded binary says {says:?}, not \"fleet {}\"",
            u.version
        );
    }
    Ok(staged)
}

/// Puts the staged binary in place of the running one, keeping the running one as the previous
/// version. The caller exits right after; nothing here stops the process.
pub fn swap(exe: &Path, staged: &Path) -> Result<()> {
    let previous = previous_path(exe);
    // An older previous from an earlier update is no longer worth keeping.
    if previous.exists() {
        std::fs::remove_file(&previous)
            .with_context(|| format!("removing the old {}", previous.display()))?;
    }
    // Windows refuses to overwrite a running exe but lets it be renamed; Unix allows both.
    std::fs::rename(exe, &previous)
        .with_context(|| format!("moving the running binary aside to {}", previous.display()))?;
    if let Err(e) = std::fs::rename(staged, exe) {
        // Put the running one back, so the node still has a binary to restart into.
        let _ = std::fs::rename(&previous, exe);
        return Err(e).with_context(|| format!("putting the new binary at {}", exe.display()));
    }
    Ok(())
}

/// Run at startup. If this binary has just replaced another (the previous one is still kept), it
/// must prove itself: connect within [`CONNECT_WITHIN`] — or exit with [`EXIT_ROLL_BACK`] for the
/// launcher to put the previous one back — and then stay up for [`PROVEN_AFTER`], after which the
/// previous one is deleted. A binary that starts but can never reach the orchestrator would
/// otherwise strand a machine nobody can get to.
pub async fn watch_new_binary(exe: PathBuf, connected: impl Fn() -> bool) {
    let previous = previous_path(&exe);
    if !previous.exists() || !enabled(&exe) {
        return;
    }
    let start = std::time::Instant::now();
    while !connected() {
        if start.elapsed() >= CONNECT_WITHIN {
            tracing::error!(
                "this binary has not reached the orchestrator in {} minutes since it was installed; asking the launcher to put the previous one back",
                CONNECT_WITHIN.as_secs() / 60
            );
            std::process::exit(EXIT_ROLL_BACK);
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    tokio::time::sleep(PROVEN_AFTER).await;
    match std::fs::remove_file(&previous) {
        Ok(()) => tracing::info!(
            "the new binary has proven itself; removed {}",
            previous.display()
        ),
        Err(e) => tracing::debug!("could not remove {}: {e}", previous.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_for_this_platform_is_the_one_the_release_publishes() {
        // The names release.yml gives its assets.
        let name = asset_name("v0.2.7");
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(
                name.as_deref(),
                Some("fleet-0.2.7-x86_64-pc-windows-msvc.exe")
            );
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(
                name.as_deref(),
                Some("fleet-0.2.7-x86_64-unknown-linux-gnu")
            );
        }
        // A version that is not a release tag names nothing.
        assert_eq!(asset_name("0.2.7"), None);
        assert_eq!(asset_name("0.1.0-dev"), None);
    }

    #[test]
    fn a_hash_is_read_from_sha256sums_exactly_as_sha256sum_writes_it() {
        let sums = "AAAA  fleet-0.2.7-x86_64-unknown-linux-gnu\n\
                    bbbb *fleet-0.2.7-x86_64-pc-windows-msvc.exe\n\
                    cccc  fleet-tray-0.2.7-x86_64-pc-windows-msvc.exe\n";
        assert_eq!(
            expected_hash(sums, "fleet-0.2.7-x86_64-unknown-linux-gnu").as_deref(),
            Some("aaaa")
        );
        assert_eq!(
            expected_hash(sums, "fleet-0.2.7-x86_64-pc-windows-msvc.exe").as_deref(),
            Some("bbbb"),
            "binary-mode names carry a *"
        );
        // A name that is a prefix of another must not match it.
        assert_eq!(expected_hash(sums, "fleet-0.2.7"), None);
        assert_eq!(expected_hash(sums, "fleet-tray-0.2.7"), None);
    }

    #[test]
    fn sha256_is_the_standard_one() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fleet-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("fleet{EXE_SUFFIX}"))
    }

    #[test]
    fn a_swap_keeps_the_running_binary_as_the_previous_one() {
        let exe = scratch("swap");
        std::fs::write(&exe, b"old").unwrap();
        let staged = staged_path(&exe, "v9.9.9");
        std::fs::write(&staged, b"new").unwrap();
        // A previous from an earlier update is replaced, not kept forever.
        std::fs::write(previous_path(&exe), b"older").unwrap();

        swap(&exe, &staged).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(previous_path(&exe)).unwrap(), b"old");
        assert!(!staged.exists());
        let _ = std::fs::remove_dir_all(exe.parent().unwrap());
    }

    #[test]
    fn a_release_the_launcher_rolled_back_is_not_taken_again() {
        let exe = scratch("rejected");
        assert!(!rejected(&exe, "v9.9.9"));
        // As the launcher writes it: the bad binary's own `--version`, with a line ending.
        std::fs::write(rejected_path(&exe), "fleet v9.9.9\r\n").unwrap();
        assert!(rejected(&exe, "v9.9.9"));
        assert!(!rejected(&exe, "v9.9.10"), "the next release is taken");
        let _ = std::fs::remove_dir_all(exe.parent().unwrap());
    }

    #[tokio::test]
    async fn a_release_from_anywhere_but_this_builds_repository_is_never_fetched() {
        for base_url in [
            "http://example.com/releases/download/v9.9.9".to_owned(),
            "https://example.com/releases/download/v9.9.9".to_owned(),
            "https://github.com/someone/else/releases/download/v9.9.9".to_owned(),
            // The right repository, but another release than the one named.
            fleet_proto::release_base_url("v9.9.8"),
        ] {
            let u = UpdateAvailable {
                version: "v9.9.9".into(),
                base_url: base_url.clone(),
            };
            let e = prepare(&u, Path::new("fleet"))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("only takes releases from"), "{base_url}: {e}");
        }
    }

    /// Against a real published release: download, check the hash, run it. Needs the internet, so
    /// it is not part of the normal run: `cargo test -p fleet-node -- --ignored real_release`.
    #[tokio::test]
    #[ignore = "downloads a real release from GitHub"]
    async fn a_real_release_is_fetched_checked_and_runs() {
        let exe = scratch("real");
        let u = UpdateAvailable {
            version: "v0.2.1".into(),
            base_url: fleet_proto::release_base_url("v0.2.1"),
        };
        let staged = prepare(&u, &exe).await.unwrap();
        assert!(staged.exists());
        let _ = std::fs::remove_dir_all(exe.parent().unwrap());
    }
}
