//! Is a given process still running? Used to watch the node and to spot a stale `status.json`.

/// What one check says about a pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Running,
    Gone,
    /// The check itself failed (say, access denied), which says nothing about the process.
    Unknown,
}

pub fn liveness(pid: u32) -> Liveness {
    #[cfg(windows)]
    {
        match Process::open(pid) {
            Ok(p) if p.has_exited() => Liveness::Gone,
            Ok(_) => Liveness::Running,
            Err(l) => l,
        }
    }
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return Liveness::Gone;
        };
        // SAFETY: signal 0 only checks that the process exists and may be signalled.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Liveness::Running;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Liveness::Gone,
            Some(libc::EPERM) => Liveness::Running,
            _ => Liveness::Unknown,
        }
    }
}

/// An open handle to a process. Holding it pins the process object, so the pid cannot be
/// reused by another process while we watch it, and the handle stays signalled after exit.
#[cfg(windows)]
pub struct Process(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a process handle is a kernel object reference, usable from any thread.
#[cfg(windows)]
unsafe impl Send for Process {}

#[cfg(windows)]
impl Process {
    pub fn open(pid: u32) -> Result<Self, Liveness> {
        use windows_sys::Win32::{
            Foundation::ERROR_INVALID_PARAMETER,
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE},
        };
        // SAFETY: plain Win32 call; ownership of the handle passes to `Process`.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !handle.is_null() {
            return Ok(Self(handle));
        }
        // Only "no such process" means it is gone; anything else (access denied) is unknown.
        let err = std::io::Error::last_os_error();
        Err(
            if err.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Liveness::Gone
            } else {
                Liveness::Unknown
            },
        )
    }

    pub fn has_exited(&self) -> bool {
        use windows_sys::Win32::{
            Foundation::WAIT_TIMEOUT, System::Threading::WaitForSingleObject,
        };
        // SAFETY: the handle is valid until drop. A process handle is signalled once it exits.
        unsafe { WaitForSingleObject(self.0, 0) != WAIT_TIMEOUT }
    }
}

#[cfg(windows)]
impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The pid of a child that has exited and been reaped.
    pub(crate) fn dead_pid() -> u32 {
        let mut cmd = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" });
        if cfg!(windows) {
            cmd.args(["/C", "exit"]);
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // Dropping the child releases its handle, so the pid really is gone (barring reuse).
        drop(child);
        pid
    }

    #[test]
    fn own_process_is_running() {
        assert_eq!(liveness(std::process::id()), Liveness::Running);
    }

    #[test]
    fn finished_child_is_gone() {
        assert_eq!(liveness(dead_pid()), Liveness::Gone);
    }

    #[cfg(windows)]
    #[test]
    fn held_handle_sees_exit() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let process = Process::open(child.id()).unwrap();
        child.wait().unwrap();
        drop(child);
        assert!(process.has_exited());
    }
}
