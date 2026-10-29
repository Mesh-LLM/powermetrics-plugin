//! A Windows job object that ends the processes in it once nothing holds it.
//!
//! `kill_on_drop` only runs when the plugin unwinds. A plugin ended outright
//! (the host killed, a console closed) leaves `nvidia-smi --loop-ms` running
//! for good on Windows, where a write to the closed pipe does not stop it. The
//! job's handle lives in the plugin process, so Windows closes it whichever
//! way the plugin ends, and the job then ends `nvidia-smi` too.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

pub(super) struct KillOnClose(OwnedHandle);

impl KillOnClose {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: no attributes and no name ask for an anonymous job; the
        // handle is checked before it is owned.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a valid job handle that nothing else owns.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer and size describe `limits`, which outlives the call.
        let set = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Puts the process behind `process` in the job.
    pub(super) fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
        // SAFETY: both handles are open for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn closing_the_job_ends_the_process_in_it() {
        // A child that would otherwise run for a minute.
        let mut child = Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let job = KillOnClose::new().unwrap();
        job.assign(child.as_raw_handle()).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "still running in the job"
        );

        drop(job);
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the child outlived its job");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
