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

#[cfg(windows)]
pub fn attach_job_object_2gb_cap() -> Result<(), String> {
    use std::mem::size_of;
    use std::ptr::null_mut;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let job = CreateJobObjectW(null_mut(), null_mut());
        if job.is_null() {
            return Err("CreateJobObjectW failed".into());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
            return Err("AssignProcessToJobObject failed".into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn attach_job_object_2gb_cap() -> Result<(), String> {
    Ok(())
}
