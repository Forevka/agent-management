//! When a process started, so a pid can be told apart from a later process that reused it.

/// An opaque, platform-specific stamp of when process `pid` started: the creation time on
/// Windows, the start time in clock ticks since boot on Linux. Only comparable with another
/// stamp from the same host. `None` when the process does not exist, cannot be queried, or the
/// platform has no cheap way to tell.
pub fn start_time(pid: u32) -> Option<u64> {
    imp::start_time(pid)
}

/// Whether `pid` is the process that `started` was taken from. With no stamp to compare (an
/// older writer, an unsupported platform, a process we may not query) this cannot be decided,
/// and `None` is returned so the caller can fall back to a plain liveness check.
pub fn is_same(pid: u32, started: Option<u64>) -> Option<bool> {
    let started = started?;
    let now = start_time(pid)?;
    Some(now == started)
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    pub fn start_time(pid: u32) -> Option<u64> {
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
            CloseHandle(handle);
            (ok != 0).then(|| {
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
            })
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn start_time(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name (field 2) is in parentheses and may contain spaces and ')', so the
        // fields are counted from the last ')'. What follows starts at field 3; the start time
        // is field 22.
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(22 - 3)?.parse().ok()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn start_time(_pid: u32) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn own_start_time_is_stable_and_identifies_this_process() {
        let me = std::process::id();
        let started = start_time(me).expect("own start time");
        assert_eq!(start_time(me), Some(started));
        assert_eq!(is_same(me, Some(started)), Some(true));
        assert_eq!(
            is_same(me, Some(started + 1)),
            Some(false),
            "another process"
        );
    }

    #[test]
    fn undecidable_without_a_stamp() {
        assert_eq!(is_same(std::process::id(), None), None);
        assert_eq!(start_time(u32::MAX - 1), None, "no such process");
    }

    /// The case that matters: the pid in a status file now belongs to some other process.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn another_process_does_not_match() {
        // Linux stamps start times in clock ticks (10 ms), so a child spawned right after this
        // test process started could share its stamp. Real pid reuse cannot happen that fast.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("ping");
            c.args(["-n", "3", "127.0.0.1"]);
            c
        } else {
            let mut c = std::process::Command::new("sleep");
            c.arg("2");
            c
        };
        let mut child = cmd.stdout(std::process::Stdio::null()).spawn().unwrap();
        let pid = child.id();
        let mine = start_time(std::process::id());
        assert_eq!(is_same(pid, mine), Some(false));
        assert_eq!(is_same(pid, start_time(pid)), Some(true));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
