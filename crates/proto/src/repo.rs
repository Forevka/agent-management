//! What the fleet accepts as a project's git repository and as a node's projects root, checked
//! by both sides: the orchestrator when the owner enters them, and the node again before it runs
//! `git` with them, so a value that got past one is still refused by the other.

/// Refuses anything that is not a plain git URL: `https://host/path`, `ssh://[user@]host/path`,
/// `user@host:path`, `file://path` or an absolute local path.
///
/// **A URL with credentials in it is refused** (`https://user:token@host/...`, and any user
/// before an https host, since a token works as a user name there): the fleet never holds git
/// credentials. Each node clones with its own machine's login (a credential manager, an ssh
/// key). The error never repeats the URL, so a token typed by mistake goes no further.
///
/// Everything else is refused too, notably git's `<transport>::<address>` form (`ext::` runs a
/// command) and anything starting with `-` (an option to `git clone`).
pub fn check_repo_url(url: &str) -> Result<(), String> {
    const KINDS: &str =
        "use https://host/path, ssh://host/path, user@host:path, file://path or an absolute path";
    let u = url.trim();
    if u.is_empty() {
        return Err("the repository URL is empty".into());
    }
    if u.len() > 500 {
        return Err("the repository URL is longer than 500 characters".into());
    }
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the repository URL must not contain spaces or control characters".into());
    }
    if u.starts_with('-') {
        return Err(format!(
            "the repository URL must not start with '-'; {KINDS}"
        ));
    }
    let credentials = || {
        Err(
            "the repository URL carries credentials (a user or token before the host). The fleet \
             never holds git credentials: log in to git on the node instead (a credential manager \
             or an ssh key), once, and give the plain URL here"
                .to_owned(),
        )
    };
    if let Some((scheme, rest)) = u.split_once("://") {
        let authority = rest.split('/').next().unwrap_or_default();
        return match scheme.to_ascii_lowercase().as_str() {
            "https" => {
                if authority.contains('@') {
                    credentials()
                } else if authority.is_empty() || rest.len() <= authority.len() + 1 {
                    Err(format!("the repository URL names no host or path; {KINDS}"))
                } else {
                    Ok(())
                }
            }
            "ssh" => {
                let (user, host) = authority.rsplit_once('@').unwrap_or(("", authority));
                if user.contains(':') {
                    credentials()
                } else if host.is_empty() || rest.len() <= authority.len() + 1 {
                    Err(format!("the repository URL names no host or path; {KINDS}"))
                } else {
                    Ok(())
                }
            }
            "file" if !rest.is_empty() => Ok(()),
            "http" => Err(
                "plain http:// would send the node's git login unencrypted; use https://".into(),
            ),
            _ => Err(format!("unsupported repository URL scheme; {KINDS}")),
        };
    }
    if u.contains("::") {
        return Err(format!(
            "git's <transport>::<address> form is not accepted; {KINDS}"
        ));
    }
    if is_absolute_path(u) {
        return Ok(());
    }
    // `user@host:path`, git's scp-like form.
    if let Some((head, path)) = u.split_once(':')
        && let Some((user, host)) = head.split_once('@')
    {
        if user.is_empty() || host.is_empty() || host.contains('/') || path.is_empty() {
            return Err(format!("the repository URL is not a git URL; {KINDS}"));
        }
        return Ok(());
    }
    // `user:password@host:path`: a password where git's scp-like form has only a user.
    if u.split_once('@')
        .is_some_and(|(user, _)| user.contains(':'))
    {
        return credentials();
    }
    Err(format!("the repository URL is not a git URL; {KINDS}"))
}

/// Refuses a projects root that is not an absolute path, on either kind of machine: the
/// orchestrator (Linux) checks a Windows node's `C:\work` as readily as a Linux node's
/// `/srv/work`. The node checks it again against its own OS.
pub fn check_projects_root(root: &str) -> Result<(), String> {
    let r = root.trim();
    if r.is_empty() {
        return Err("the projects root is empty".into());
    }
    if r.len() > 400 {
        return Err("the projects root is longer than 400 characters".into());
    }
    if r.chars().any(char::is_control) {
        return Err("the projects root must not contain control characters".into());
    }
    if !is_absolute_path(r) {
        return Err(format!(
            "the projects root must be an absolute path (C:\\work or /srv/work), not {r:?}"
        ));
    }
    Ok(())
}

/// `/x`, `C:\x`, `C:/x` or `\\server\share`: absolute on Unix or on Windows.
fn is_absolute_path(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with(r"\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_git_urls_are_accepted() {
        for ok in [
            "https://github.com/owner/repo.git",
            "https://gitlab.example.com/group/sub/repo",
            "ssh://git@github.com/owner/repo.git",
            "ssh://host.example:2222/srv/repo",
            "git@github.com:owner/repo.git",
            "file:///tmp/repo.git",
            "/srv/git/repo.git",
            r"C:\Users\me\AppData\Local\Temp\bare.git",
            "C:/repos/bare.git",
            r"\\server\share\repo.git",
            "  https://github.com/owner/repo  ",
        ] {
            assert_eq!(check_repo_url(ok), Ok(()), "{ok}");
        }
    }

    #[test]
    fn credentials_are_refused_without_repeating_them() {
        for creds in [
            "https://user:ghp_secret123@github.com/owner/repo.git",
            "https://ghp_secret123@github.com/owner/repo.git",
            "HTTPS://oauth2:glpat-secret123@gitlab.com/g/r.git",
            "ssh://git:hunter2secret@host/repo.git",
            "user:hunter2secret@host:repo.git",
        ] {
            let e = check_repo_url(creds).unwrap_err();
            assert!(e.contains("credentials"), "{creds}: {e}");
            assert!(e.contains("log in to git on the node"), "{e}");
            assert!(!e.contains("secret123") && !e.contains("hunter2"), "{e}");
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for bad in [
            "",
            "   ",
            "repo",
            "relative/path/repo",
            "./repo",
            "http://github.com/owner/repo",
            "ftp://host/repo",
            "ext::sh -c touch% /tmp/pwned",
            "ext::sh",
            "fd::17",
            "--upload-pack=touch /tmp/x",
            "-u/x",
            "https://",
            "https://github.com",
            "ssh://",
            "file://",
            "git@:repo",
            "@host:repo",
            "git@host:",
            "https://github.com/owner/repo\n--config",
        ] {
            assert!(check_repo_url(bad).is_err(), "{bad:?}");
        }
        assert!(check_repo_url(&format!("https://h/{}", "a".repeat(600))).is_err());
    }

    #[test]
    fn a_projects_root_is_absolute_on_some_os() {
        for ok in ["/srv/work", r"C:\work", "D:/fleet-projects", r"\\nas\work"] {
            assert_eq!(check_projects_root(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "work",
            "./work",
            "~/fleet-projects",
            "C:work",
            "/srv/\nwork",
        ] {
            assert!(check_projects_root(bad).is_err(), "{bad:?}");
        }
    }
}
