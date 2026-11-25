pub const HARD_CAP_BYTES: usize = 2 * 1024 * 1024 * 1024;
pub const GOVERNOR_WARN_MIB: usize = 1200;
pub const GOVERNOR_TRIM_MIB: usize = 1550;
pub const GOVERNOR_SHED_MIB: usize = 1750;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    Normal,
    Warn,
    TrimCaches,
    ShedHeavyWork,
}

pub fn classify_pressure(rss_mib: usize) -> PressureLevel {
    if rss_mib >= GOVERNOR_SHED_MIB {
        PressureLevel::ShedHeavyWork
    } else if rss_mib >= GOVERNOR_TRIM_MIB {
        PressureLevel::TrimCaches
    } else if rss_mib >= GOVERNOR_WARN_MIB {
        PressureLevel::Warn
    } else {
        PressureLevel::Normal
    }
}

/// Owns the job handle. Keep alive for as long as the limit is required.
#[cfg(windows)]
pub struct JobGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); }
    }
}

#[cfg(not(windows))]
pub struct JobGuard;

#[cfg(windows)]
pub fn attach_job_object_2gb_cap() -> Result<JobGuard, String> {
    use std::mem::size_of;
    use std::ptr::null_mut;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let job = CreateJobObjectW(null_mut(), null_mut());
        if job.is_null() {
            return Err("CreateJobObjectW failed".into());
        }
        let guard = JobGuard(job);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = HARD_CAP_BYTES;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            return Err("SetInformationJobObject failed".into());
        }
        if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            return Err(format!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error()));
        }
        Ok(guard)
    }
}

#[cfg(not(windows))]
pub fn attach_job_object_2gb_cap() -> Result<JobGuard, String> {
    Err("Windows job objects are unavailable on this platform".into())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::System::JobObjects::{QueryInformationJobObject, JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_PROCESS_MEMORY};

    #[test]
    fn real_job_handle_has_limit() {
        if std::env::var_os("MEMB_JOB_TEST_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::real_job_handle_has_limit"])
                .env("MEMB_JOB_TEST_CHILD", "1").status().unwrap();
            assert!(status.success());
            return;
        }
        let guard = attach_job_object_2gb_cap().unwrap();
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { QueryInformationJobObject(guard.0, JobObjectExtendedLimitInformation, &mut info as *mut _ as *mut _, std::mem::size_of_val(&info) as u32, std::ptr::null_mut()) };
        assert_ne!(ok, 0);
        assert_ne!(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_PROCESS_MEMORY, 0);
        assert_eq!(info.ProcessMemoryLimit, HARD_CAP_BYTES);
    }
}
