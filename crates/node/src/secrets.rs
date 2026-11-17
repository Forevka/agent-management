//! Secret files for agents: written outside the project, readable one key at a time.
//!
//! The orchestrator sends a task's secrets with its spawn message. They land in
//! `<data>/secrets/<task>/<name>.env`, never inside the repository, and are deleted when the
//! agent exits. Agents are told to read single keys with `fleet secret get` or to run commands
//! under `fleet secret exec`, so whole files never end up in a transcript.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use fleet_proto::SecretFile;

pub fn task_dir(data_dir: &Path, task_id: &str) -> PathBuf {
    data_dir.join("secrets").join(sanitize(task_id))
}

/// Writes the files and returns their paths. Existing files for the task are replaced.
pub fn write(data_dir: &Path, task_id: &str, files: &[SecretFile]) -> Result<Vec<PathBuf>> {
    let dir = task_dir(data_dir, task_id);
    if files.is_empty() {
        return Ok(vec![]);
    }
    std::fs::create_dir_all(&dir)?;
    restrict(&dir, true)?;
    let mut paths = Vec::new();
    for f in files {
        if f.name.is_empty()
            || !f
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("invalid secret file name {:?}", f.name);
        }
        let path = dir.join(format!("{}.env", f.name));
        std::fs::write(&path, &f.content)?;
        restrict(&path, false)?;
        paths.push(path);
    }
    Ok(paths)
}

pub fn remove(data_dir: &Path, task_id: &str) {
    let dir = task_dir(data_dir, task_id);
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        tracing::warn!("could not remove secrets for {task_id}: {e}");
    }
}

/// Parses `KEY=value` lines. Blank lines and `#` comments are skipped; values may be quoted.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            out.insert(k.trim().to_owned(), v.to_owned());
        }
    }
    out
}

pub fn get(file: &Path, key: &str) -> Result<String> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    parse_env(&text)
        .remove(key)
        .with_context(|| format!("{key} is not in {}", file.display()))
}

/// Runs a command with the variables of the given files added to its environment.
pub fn exec(files: &[PathBuf], cmd: &[String]) -> Result<i32> {
    let Some((program, args)) = cmd.split_first() else {
        bail!("no command given")
    };
    let mut command = std::process::Command::new(program);
    command.args(args);
    for f in files {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        command.envs(parse_env(&text));
    }
    let status = command
        .status()
        .with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Owner-only permissions on Unix. On Windows the data directory lives in the service user's
/// profile, whose default ACL already excludes other users.
fn restrict(path: &Path, dir: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if dir { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = (path, dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_get_remove() {
        let data = std::env::temp_dir().join(format!("fleet-secrets-{}", std::process::id()));
        let files = vec![SecretFile {
            name: "jira".into(),
            content: "# c\nJIRA_URL=https://x\nexport JIRA_TOKEN=\"t o k\"\n".into(),
        }];
        let paths = write(&data, "proj-1", &files).unwrap();
        assert_eq!(get(&paths[0], "JIRA_TOKEN").unwrap(), "t o k");
        assert_eq!(get(&paths[0], "JIRA_URL").unwrap(), "https://x");
        assert!(get(&paths[0], "NOPE").is_err());
        assert!(
            write(
                &data,
                "proj-1",
                &[SecretFile {
                    name: "../x".into(),
                    content: String::new()
                }]
            )
            .is_err()
        );
        remove(&data, "proj-1");
        assert!(!task_dir(&data, "proj-1").exists());
        let _ = std::fs::remove_dir_all(&data);
    }
}
