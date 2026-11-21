//! Free disk space where this node's sessions write, for the orchestrator's [`DiskReport`].
//!
//! Claude Code writes every session, subagent and transcript under its config dir, and the
//! projects (with their worktrees) grow on their own disks. When one of them fills up, runs fail
//! one after another and a session's transcript is cut short, so resuming it loses context. The
//! node measures, the orchestrator decides: thresholds, warnings and holding new runs are all
//! on its side.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use fleet_proto::{DiskReport, DiskVolume, FolderSize, ProjectMapping};

/// How many of the largest session folders a report names.
const BIGGEST: usize = 5;
/// Upper bound on the entries looked at when sizing one session folder, so a pathological tree
/// cannot keep a blocking thread busy for minutes. Per folder, so one huge folder cannot starve
/// the others of a size; past it a folder's size is a lower bound, and a folder that big is one
/// of the biggest anyway.
const MAX_ENTRIES_PER_FOLDER: usize = 100_000;
/// Upper bound on the time spent sizing all the session folders together. The free space is what
/// decides anything and is measured first; the biggest folders only say where to clean up, so a
/// slow disk or hundreds of worktree folders cut this list short rather than the report.
const WALK_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where Claude Code keeps its sessions for the user this node runs as: `CLAUDE_CONFIG_DIR` if
/// set (the agents inherit the node's environment, so they use the same one), else `~/.claude`.
pub fn claude_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".claude"))
}

/// Measures the Claude config dir and every mapped project. A path that cannot be measured is
/// left out (and logged): a partial report is still worth more than none.
pub fn report(claude_dir: &Path, projects: &[ProjectMapping]) -> DiskReport {
    let mut probes = Vec::new();
    match probe(claude_dir) {
        Ok(p) => probes.push((Target::ClaudeDir, p)),
        Err(e) => tracing::warn!("cannot measure free space at {}: {e}", claude_dir.display()),
    }
    for m in projects {
        match probe(Path::new(&m.path)) {
            Ok(p) => probes.push((Target::Project(m.project.clone()), p)),
            Err(e) => tracing::warn!("cannot measure free space at {}: {e}", m.path),
        }
    }
    DiskReport {
        claude_dir: claude_dir.display().to_string(),
        volumes: group(probes),
        biggest: biggest_folders(&claude_dir.join("projects"), BIGGEST),
    }
}

/// What a measured path is to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    ClaudeDir,
    Project(String),
}

/// One measured path: the volume it is on and that volume's space.
#[derive(Debug, Clone, PartialEq)]
struct Probe {
    volume: String,
    free: u64,
    total: u64,
}

/// Folds measured paths into one entry per volume, in the order the volumes were first seen
/// (the Claude config dir's comes first). The space of a volume is the last one measured on it.
fn group(probes: Vec<(Target, Probe)>) -> Vec<DiskVolume> {
    let mut out: Vec<DiskVolume> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (target, p) in probes {
        let i = *index.entry(p.volume.clone()).or_insert_with(|| {
            out.push(DiskVolume {
                volume: p.volume.clone(),
                free: p.free,
                total: p.total,
                claude_dir: false,
                projects: Vec::new(),
            });
            out.len() - 1
        });
        let v = &mut out[i];
        v.free = p.free;
        v.total = p.total;
        match target {
            Target::ClaudeDir => v.claude_dir = true,
            Target::Project(slug) => {
                if !v.projects.contains(&slug) {
                    v.projects.push(slug);
                }
            }
        }
    }
    out
}

/// The `n` largest subfolders of `dir`, largest first. Nothing if `dir` does not exist yet.
fn biggest_folders(dir: &Path, n: usize) -> Vec<FolderSize> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + WALK_LIMIT;
    let mut sizes: Vec<FolderSize> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .take_while(|_| std::time::Instant::now() < deadline)
        .map(|e| FolderSize {
            name: e.file_name().to_string_lossy().into_owned(),
            bytes: tree_size(&e.path(), MAX_ENTRIES_PER_FOLDER, deadline),
        })
        .collect();
    sizes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    sizes.truncate(n);
    sizes
}

/// Bytes of the files under `dir`, without following symlinks (a link out of the tree must not
/// count another disk, or loop). Looks at no more than `budget` entries, and stops at `deadline`.
fn tree_size(dir: &Path, mut budget: usize, deadline: std::time::Instant) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            if budget == 0 || (budget.is_multiple_of(1024) && std::time::Instant::now() >= deadline)
            {
                return total;
            }
            budget -= 1;
            // `DirEntry::metadata` does not follow symlinks.
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Bytes this user may still write on the volume `path` is (or would be) on.
pub fn free_at(path: &Path) -> std::io::Result<u64> {
    probe(path).map(|p| p.free)
}

/// The volume `path` is on and its space. A path that does not exist yet (a fresh node has no
/// `~/.claude`) is measured at its nearest existing ancestor, where it would be created.
fn probe(path: &Path) -> std::io::Result<Probe> {
    let existing = path
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no existing ancestor"))?;
    let (free, total) = sys::space(existing)?;
    Ok(Probe {
        volume: sys::volume(existing)?,
        free,
        total,
    })
}

#[cfg(windows)]
mod sys {
    use std::{os::windows::ffi::OsStrExt, path::Path};

    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumePathNameW};

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// Bytes available to this user (quotas included) and the volume's size.
    pub fn space(path: &Path) -> std::io::Result<(u64, u64)> {
        let p = wide(path);
        let (mut free, mut total, mut all_free) = (0u64, 0u64, 0u64);
        // SAFETY: `p` is a NUL-terminated UTF-16 string that outlives the call, and the three
        // out-pointers point at live, writable u64s.
        let ok = unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &mut free, &mut total, &mut all_free) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((free, total))
    }

    /// The volume's mount point, e.g. `C:\` or a folder a volume is mounted on.
    pub fn volume(path: &Path) -> std::io::Result<String> {
        let p = wide(path);
        let mut buf = vec![0u16; 1024];
        // SAFETY: `p` is NUL-terminated; `buf` is writable for the length passed.
        let ok = unsafe { GetVolumePathNameW(p.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len]))
    }
}

#[cfg(unix)]
mod sys {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
        path::Path,
    };

    /// Bytes available to unprivileged users and the filesystem's size.
    // The field types of `statvfs` differ between platforms (u64 on Linux, u32 on some others),
    // so the casts are needed somewhere even where they are no-ops.
    #[allow(clippy::unnecessary_cast)]
    pub fn space(path: &Path) -> std::io::Result<(u64, u64)> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: an all-zero `statvfs` is a valid value of this plain C struct.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is NUL-terminated and `st` is a valid, writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let frsize = st.f_frsize as u64;
        Ok((
            (st.f_bavail as u64).saturating_mul(frsize),
            (st.f_blocks as u64).saturating_mul(frsize),
        ))
    }

    /// The mount point the path is on: its highest ancestor on the same device.
    pub fn volume(path: &Path) -> std::io::Result<String> {
        let path = path.canonicalize()?;
        let dev = std::fs::metadata(&path)?.dev();
        let mut mount = path.as_path();
        for a in path.ancestors().skip(1) {
            match std::fs::metadata(a) {
                Ok(m) if m.dev() == dev => mount = a,
                _ => break,
            }
        }
        Ok(mount.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_on(volume: &str, free: u64) -> Probe {
        Probe {
            volume: volume.into(),
            free,
            total: 100,
        }
    }

    #[test]
    fn paths_on_one_volume_are_reported_once() {
        let volumes = group(vec![
            (Target::ClaudeDir, probe_on("C:\\", 10)),
            (Target::Project("a".into()), probe_on("D:\\", 50)),
            (Target::Project("b".into()), probe_on("C:\\", 9)),
            (Target::Project("b".into()), probe_on("C:\\", 9)),
        ]);
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].volume, "C:\\");
        assert!(volumes[0].claude_dir);
        assert_eq!(volumes[0].projects, vec!["b".to_string()]);
        // The latest measurement of a volume wins.
        assert_eq!(volumes[0].free, 9);
        assert_eq!(volumes[1].volume, "D:\\");
        assert!(!volumes[1].claude_dir);
        assert_eq!(volumes[1].projects, vec!["a".to_string()]);
    }

    #[test]
    fn the_biggest_session_folders_come_first() {
        let root = std::env::temp_dir().join(format!("fleet-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, files) in [("small", 1), ("big", 3), ("mid", 2)] {
            let dir = root.join(name).join("nested");
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..files {
                std::fs::write(dir.join(format!("{i}.jsonl")), vec![b'x'; 1000]).unwrap();
            }
        }
        std::fs::write(root.join("loose-file"), b"not a folder").unwrap();
        let biggest = biggest_folders(&root, 2);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            biggest,
            vec![
                FolderSize {
                    name: "big".into(),
                    bytes: 3000
                },
                FolderSize {
                    name: "mid".into(),
                    bytes: 2000
                },
            ]
        );
    }

    #[test]
    fn a_missing_folder_has_no_biggest() {
        assert!(biggest_folders(Path::new("/definitely/not/here/fleet"), 5).is_empty());
    }

    #[test]
    fn real_space_is_measured_at_the_nearest_existing_ancestor() {
        let missing = std::env::temp_dir()
            .join("fleet-disk-not-created-yet")
            .join("x");
        let p = probe(&missing).unwrap();
        assert!(p.total > 0);
        assert!(p.free <= p.total);
        assert!(!p.volume.is_empty());
        // Free space moves while other things write; the volume and its size do not.
        let at = probe(&std::env::temp_dir()).unwrap();
        assert_eq!((p.volume, p.total), (at.volume, at.total));
    }
}
