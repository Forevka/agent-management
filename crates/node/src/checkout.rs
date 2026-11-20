//! Projects the owner assigned to this node, cloned when a task first needs them.
//!
//! The node clones with whatever git login this machine already has (a credential manager, an
//! ssh key): the fleet never holds git credentials. It runs git so that it can never stop to
//! ask anyone (no terminal prompt, no credential dialog, ssh in batch mode), for at most
//! [`CLONE_LIMIT`]. It never deletes or overwrites what it did not create: a directory already
//! at the target is adopted when it is a checkout of the same repository and refused otherwise.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::io::AsyncReadExt;

/// How long one clone may take before it is stopped. Large repositories over a slow link take
/// minutes; one that takes longer than this is hung on something nobody will answer.
pub const CLONE_LIMIT: Duration = Duration::from_secs(20 * 60);

/// `~/fleet-projects` (`%USERPROFILE%\fleet-projects` on Windows): where clones go when the
/// owner set no projects root for this node.
pub fn default_root() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("fleet-projects"))
}

/// What to do with the directory an assigned project would be cloned into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Nothing is there: clone into it.
    Clone,
    /// A checkout of the same repository is already there: map it as it is.
    Adopt,
    /// Something else is there, which is never touched. Why, for the owner.
    Refuse(String),
}

/// Decides from what is at `dir`: whether it exists, and, when it is a git checkout, its
/// `origin`. Kept free of the file system so every case is tested.
pub fn decide(dir: &Path, exists: bool, origin: Option<&str>, url: &str) -> Target {
    match (exists, origin) {
        (false, _) => Target::Clone,
        (true, Some(o)) if same_repo(o, url) => Target::Adopt,
        // The other origin is not quoted: a URL someone set up by hand may carry a token.
        (true, Some(_)) => Target::Refuse(format!(
            "{} is already a git checkout of another repository, not {url}. Nothing was changed: \
             map the project by hand, or set another projects root for this node.",
            dir.display()
        )),
        (true, None) => Target::Refuse(format!(
            "{} already exists and is not a git checkout of {url}. Nothing was changed: move it \
             away, map the project by hand, or set another projects root for this node.",
            dir.display()
        )),
    }
}

/// Whether two remote URLs name the same repository: equal once a trailing `/` or `.git` is
/// dropped, slashes are one kind and case is ignored (hosts are case-insensitive, and so are the
/// paths of the hosting services and of Windows).
pub fn same_repo(a: &str, b: &str) -> bool {
    fn norm(u: &str) -> String {
        let u = u.trim().replace('\\', "/");
        let u = u.trim_end_matches('/');
        let u = u.strip_suffix(".git").unwrap_or(u).trim_end_matches('/');
        u.to_ascii_lowercase()
    }
    norm(a) == norm(b)
}

/// Makes `<root>/<slug>` a checkout of `url`, cloning it if nothing is there, and returns it.
/// The error is a sentence for the owner saying what to do.
pub async fn ensure(root: &Path, slug: &str, url: &str) -> Result<PathBuf, String> {
    // Checked again here, whatever the orchestrator checked: a URL with a token in it never
    // reaches git, and nor does anything git would read as an option or a command.
    fleet_proto::repo::check_repo_url(url)?;
    crate::config::validate_slug(slug).map_err(|e| format!("{e:#}"))?;
    if !root.is_absolute() {
        return Err(format!(
            "the projects root {} is not an absolute path on this machine; set one in the node's page",
            root.display()
        ));
    }
    let url = url.trim();
    std::fs::create_dir_all(root)
        .map_err(|e| format!("cannot create the projects root {}: {e}", root.display()))?;
    let dir = root.join(slug);
    let exists = dir.exists();
    let origin = if exists && dir.join(".git").exists() {
        git_origin(&dir).await
    } else {
        None
    };
    match decide(&dir, exists, origin.as_deref(), url) {
        // A clone cut short (the node stopped mid-way) already names its origin but has no
        // commit checked out: not something to start work in.
        Target::Adopt if !has_head(&dir).await => Err(format!(
            "{} looks like an unfinished clone of {url} (no commit is checked out). Nothing was \
             changed: delete it and the next task clones it again.",
            dir.display()
        )),
        Target::Adopt => Ok(dir),
        Target::Refuse(why) => Err(why),
        Target::Clone => {
            clone(url, &dir).await?;
            Ok(dir)
        }
    }
}

/// `git remote get-url origin` of a checkout, if it has one.
async fn git_origin(dir: &Path) -> Option<String> {
    let mut cmd = git();
    cmd.arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|o| !o.is_empty())
}

/// Whether a checkout has a commit checked out.
async fn has_head(dir: &Path) -> bool {
    let mut cmd = git();
    cmd.arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        tokio::time::timeout(Duration::from_secs(30), cmd.status()).await,
        Ok(Ok(s)) if s.success()
    )
}

async fn clone(url: &str, dir: &Path) -> Result<(), String> {
    let by_hand = format!(
        "Log in to git on this machine (run `git clone {url}` once by hand) and the next task will clone it."
    );
    let mut cmd = git();
    cmd.arg("clone")
        .arg("--")
        .arg(url)
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "git could not be started on this machine ({e}): install git, make sure the node finds it \
             on its PATH, and log in once; the next task will clone the project."
        )
    })?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let read = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let status = match tokio::time::timeout(CLONE_LIMIT, child.wait()).await {
        Ok(status) => status.map_err(|e| format!("waiting for git failed: {e}"))?,
        Err(_) => {
            // Killed and reaped before the half-made directory is removed, so nothing still
            // holds its files open.
            let _ = child.kill().await;
            let _ = read.await;
            remove_partial(dir);
            return Err(format!(
                "cloning {url} took over {} minutes and was stopped. {by_hand}",
                CLONE_LIMIT.as_secs() / 60
            ));
        }
    };
    let stderr = String::from_utf8_lossy(&read.await.unwrap_or_default()).into_owned();
    if status.success() {
        return Ok(());
    }
    // Git removes what it created when a clone fails, but not when it is killed by something
    // else; the directory did not exist before, so whatever is there now is the clone's.
    remove_partial(dir);
    Err(format!(
        "git could not clone {url} on this machine: {}. {by_hand}",
        last_lines(&stderr)
    ))
}

/// Git that never waits for a person: no terminal prompt, no credential manager dialog, and ssh
/// in batch mode (unless this machine set its own ssh command, which is kept), with no console
/// window of its own on Windows.
fn git() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .kill_on_drop(true);
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

fn remove_partial(dir: &Path) {
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::warn!(
            "could not remove the unfinished clone {}: {e}",
            dir.display()
        );
    }
}

/// The end of git's error output, where the reason is: its last few lines, at most ~600 chars.
fn last_lines(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(4)..].join(" / ");
    if tail.is_empty() {
        return "git gave no reason".into();
    }
    match tail.char_indices().rev().nth(599) {
        Some((i, _)) => format!("…{}", &tail[i..]),
        None => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_directory_is_cloned_into_adopted_or_left_alone() {
        let dir = Path::new("/w/demo");
        let url = "https://github.com/o/demo.git";
        assert_eq!(decide(dir, false, None, url), Target::Clone);
        assert_eq!(
            decide(dir, true, Some("https://GitHub.com/o/demo/"), url),
            Target::Adopt
        );
        let Target::Refuse(why) = decide(
            dir,
            true,
            Some("https://user:tok3n@github.com/o/other.git"),
            url,
        ) else {
            panic!("another repository is refused");
        };
        assert!(why.contains("another repository"), "{why}");
        assert!(
            !why.contains("tok3n"),
            "another origin is never quoted: {why}"
        );
        let Target::Refuse(why) = decide(dir, true, None, url) else {
            panic!("a directory that is no checkout is refused");
        };
        assert!(why.contains("not a git checkout"), "{why}");
    }

    #[test]
    fn remotes_compare_without_their_decoration() {
        assert!(same_repo("git@github.com:o/r.git", "git@github.com:o/r"));
        assert!(same_repo(r"C:\tmp\bare.git", "c:/tmp/bare.git/"));
        assert!(!same_repo("https://h/o/r", "https://h/o/r2"));
    }

    #[test]
    fn git_s_reason_is_its_last_lines() {
        let err = "Cloning into 'x'...\n\nremote: nope\nfatal: Authentication failed\n";
        assert_eq!(
            last_lines(err),
            "Cloning into 'x'... / remote: nope / fatal: Authentication failed"
        );
        assert_eq!(last_lines(""), "git gave no reason");
        assert!(last_lines(&"x".repeat(5000)).chars().count() <= 601);
    }

    fn run_git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[tokio::test]
    async fn an_existing_directory_is_adopted_or_refused_and_never_changed() {
        let root = std::env::temp_dir().join(format!("fleet-checkout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let url = "https://example.invalid/o/demo.git";

        // A checkout of the same repository is mapped as it is.
        let same = root.join("same");
        std::fs::create_dir_all(&same).unwrap();
        if !run_git(&same, &["init", "-q"]) {
            eprintln!("skipped: git not available");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert!(run_git(&same, &["remote", "add", "origin", url]));
        // Until a commit is checked out it is an unfinished clone, and left alone.
        let e = ensure(&root, "same", url).await.unwrap_err();
        assert!(e.contains("unfinished clone"), "{e}");
        assert!(run_git(
            &same,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x"
            ]
        ));
        assert_eq!(ensure(&root, "same", url).await, Ok(same.clone()));

        // A checkout of something else, and a plain directory: refused, untouched.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert!(run_git(&other, &["init", "-q"]));
        assert!(run_git(
            &other,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/o/other.git"
            ]
        ));
        let e = ensure(&root, "other", url).await.unwrap_err();
        assert!(e.contains("another repository"), "{e}");
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("keep.txt"), "mine").unwrap();
        let e = ensure(&root, "plain", url).await.unwrap_err();
        assert!(e.contains("not a git checkout"), "{e}");
        assert_eq!(
            std::fs::read_to_string(plain.join("keep.txt")).unwrap(),
            "mine"
        );

        // A clone that fails says what to do and leaves nothing behind.
        let missing = root.join("no-such-origin.git").display().to_string();
        let e = ensure(&root, "absent", &missing).await.unwrap_err();
        assert!(e.contains("git could not clone"), "{e}");
        assert!(e.contains("Log in to git on this machine"), "{e}");
        assert!(!root.join("absent").exists());

        // Refused before git ever runs.
        let e = ensure(&root, "absent", "https://u:tok@example.invalid/o/r.git")
            .await
            .unwrap_err();
        assert!(e.contains("credentials"), "{e}");
        assert!(ensure(Path::new("relative"), "absent", url).await.is_err());
        assert!(ensure(&root, "../escape", url).await.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
