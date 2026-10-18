//! Each PTY child gets its own unnamed job. No PID-based process-tree lookup.
use anyhow::{Context, Result};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    },
};

pub struct WindowsJob(OwnedHandle);

impl WindowsJob {
    pub fn attach(child: &dyn portable_pty::Child) -> Result<Self> {
        let process = child.as_raw_handle().context("PTY child has no process handle")?;
        // SAFETY: no name/security descriptor; ownership is transferred immediately.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error()).context("CreateJobObjectW");
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw.cast()) });
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: handles are live; the struct size matches the requested information class.
        if unsafe {
            SetInformationJobObject(
                job.handle(), JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0 {
            return Err(std::io::Error::last_os_error()).context("SetInformationJobObject");
        }
        if unsafe { AssignProcessToJobObject(job.handle(), process.cast()) } == 0 {
            return Err(std::io::Error::last_os_error()).context("AssignProcessToJobObject");
        }
        Ok(job)
    }

    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle().cast()
    }

    pub fn terminate(&self) -> Result<()> {
        // Only members of this particular job are affected, never a reused PID.
        if unsafe { TerminateJobObject(self.handle(), 1) } == 0 {
            return Err(std::io::Error::last_os_error()).context("TerminateJobObject");
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.handle(), JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32, std::ptr::null_mut(),
                )
            } == 0 {
                return Err(std::io::Error::last_os_error()).context("QueryInformationJobObject");
            }
            if info.ActiveProcesses == 0 { return Ok(()); }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("Termination requested but job processes have not exited yet");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
