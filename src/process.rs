//! Spawning a command with its whole process tree contained, so a timeout can stop everything it
//! started: a Windows Job Object with `KILL_ON_JOB_CLOSE`, or a Unix process group.

use std::process::{Child, Command};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Stops a contained process tree. Dropping it on Windows closes the job, which also ends the tree.
#[cfg(windows)]
pub struct Canceller {
    job: HANDLE,
}

// The job handle is only used to terminate or close the job, which Windows allows from any thread.
#[cfg(windows)]
unsafe impl Send for Canceller {}

#[cfg(windows)]
impl Canceller {
    /// Terminate every process in the tree now. A failure means the tree already exited.
    pub fn cancel_force(&self) {
        unsafe {
            let _ = windows::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for Canceller {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

/// Stops a contained process tree by signalling its process group.
#[cfg(not(windows))]
pub struct Canceller {
    pgid: u32,
}

#[cfg(not(windows))]
impl Canceller {
    /// Kill every process in the group now. A failure means the group already exited.
    pub fn cancel_force(&self) {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{}", self.pgid))
            .status();
    }
}

/// Create a kill-on-close Job Object and assign `child` to it. The caller must hold the returned
/// `Canceller` for the child's lifetime.
#[cfg(windows)]
fn assign_to_new_job(child: &Child) -> std::io::Result<Canceller> {
    unsafe {
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) {
            let _ = windows::Win32::Foundation::CloseHandle(job);
            return Err(std::io::Error::other(e.to_string()));
        }
        if let Err(e) = AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) {
            let _ = windows::Win32::Foundation::CloseHandle(job);
            return Err(std::io::Error::other(e.to_string()));
        }
        Ok(Canceller { job })
    }
}

/// Resume the main thread of a process created suspended. Errors when no thread was resumed, so
/// the caller can kill a child that would otherwise hang forever.
#[cfg(windows)]
fn resume_process(pid: u32) -> std::io::Result<()> {
    use windows::Win32::Foundation::{CloseHandle, FALSE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut resumed = false;
        if Thread32First(snap, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid {
                    if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, FALSE, entry.th32ThreadID) {
                        // ResumeThread returns the previous suspend count, or u32::MAX on failure.
                        let prev = ResumeThread(h);
                        let _ = CloseHandle(h);
                        if prev != u32::MAX {
                            resumed = true;
                        }
                    }
                }
                if Thread32Next(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        if resumed {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "could not resume the suspended child's main thread",
            ))
        }
    }
}

/// Spawn `command` with its whole tree contained, without a race.
///
/// On Windows the child starts suspended and joins the Job Object before it runs, so every process
/// it later starts is born inside the job. On any failure the child is killed. On Unix the child
/// leads a new process group that the `Canceller` signals.
pub fn spawn_contained_command(mut command: Command) -> std::io::Result<(Child, Canceller)> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let canceller = match assign_to_new_job(&child) {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        if let Err(e) = resume_process(child.id()) {
            let _ = child.kill();
            drop(canceller);
            return Err(e);
        }
        Ok((child, canceller))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let child = command.spawn()?;
        let pgid = child.id();
        Ok((child, Canceller { pgid }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[cfg(windows)]
    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", script]);
        cmd
    }

    #[cfg(not(windows))]
    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn a_contained_process_runs_to_completion() {
        let mut cmd = shell("exit 7");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let (mut child, _canceller) = spawn_contained_command(cmd).expect("spawn contained");
        assert_eq!(child.wait().expect("wait").code(), Some(7));
    }

    #[test]
    fn cancelling_kills_the_contained_tree() {
        let script = if cfg!(windows) {
            "ping -n 20 127.0.0.1 >nul"
        } else {
            "sleep 20"
        };
        let mut cmd = shell(script);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let (mut child, canceller) = spawn_contained_command(cmd).expect("spawn contained");
        canceller.cancel_force();
        assert!(!child.wait().expect("wait").success());
    }
}
