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

/// True while background work (the daily routine, curation, sweeps) may start at this
/// memory use: everything but `ShedHeavyWork`.
pub fn allow_background_jobs(level: PressureLevel) -> bool {
    level != PressureLevel::ShedHeavyWork
}

/// True when caches and the idle embedding model should be dropped now.
pub fn should_trim_caches(level: PressureLevel) -> bool {
    matches!(level, PressureLevel::TrimCaches | PressureLevel::ShedHeavyWork)
}

/// This process's private memory in MiB (what it has committed for itself), the
/// number the memory governor compares with its thresholds. `None` if unavailable.
pub fn process_private_mib() -> Option<usize> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters as *mut _ as *mut _, counters.cb) == 0 {
            return None;
        }
        return Some(counters.PrivateUsage / (1024 * 1024));
    }
    #[cfg(not(windows))]
    None
}

/// What the hardware prober read. `Default` is a modest machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hardware {
    pub avx512: bool,
    pub avx2_fma: bool,
    /// Physical cores, never hyperthreads.
    pub physical_cores: usize,
    pub total_ram_mib: usize,
}

impl Default for Hardware {
    fn default() -> Self {
        Self { avx512: false, avx2_fma: false, physical_cores: 4, total_ram_mib: 8 * 1024 }
    }
}

/// The in-process embedding model's tier. There is no GPU tier: memb's embedder is a
/// CPU-only ONNX model, so a GPU could not be used (the Phase 2 `GpuHigh` is replaced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    CpuLite,
    CpuBalanced,
}

impl Tier {
    /// Seconds the model may sit unused before it is unloaded and its memory returned.
    pub fn idle_unload_secs(self) -> u64 {
        match self {
            Tier::CpuLite => 300,
            Tier::CpuBalanced => 600,
        }
    }
}

/// Phase 2: `CpuBalanced` needs AVX2 (with FMA) or AVX-512, at least 8 physical cores
/// and at least 16 GB of RAM; anything less is `CpuLite`.
pub fn choose_tier(hw: &Hardware) -> Tier {
    if (hw.avx512 || hw.avx2_fma) && hw.physical_cores >= 8 && hw.total_ram_mib >= 16 * 1024 {
        Tier::CpuBalanced
    } else {
        Tier::CpuLite
    }
}

/// Reads this machine: CPU features, physical cores and installed RAM.
pub fn probe_hardware() -> Hardware {
    let mut hw = Hardware::default();
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        hw.avx512 = std::arch::is_x86_feature_detected!("avx512f");
        hw.avx2_fma = std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma");
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, RelationProcessorCore, MEMORYSTATUSEX,
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
        };
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) != 0 {
            hw.total_ram_mib = (status.ullTotalPhys / (1024 * 1024)) as usize;
        }
        let mut length = 0u32;
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut length);
        if length > 0 {
            let mut buffer = vec![0u8; length as usize];
            if GetLogicalProcessorInformationEx(RelationProcessorCore, buffer.as_mut_ptr() as *mut _, &mut length) != 0 {
                let (mut offset, mut cores) = (0usize, 0usize);
                while offset < length as usize {
                    let record = &*(buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
                    if record.Size == 0 {
                        break;
                    }
                    cores += 1;
                    offset += record.Size as usize;
                }
                if cores > 0 {
                    hw.physical_cores = cores;
                }
            }
        }
    }
    hw
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
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let job = CreateJobObjectW(null_mut(), null_mut());
        if job.is_null() {
            return Err("CreateJobObjectW failed".into());
        }
        let guard = JobGuard(job);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // The cap is for the whole job: this process and everything it starts after
        // this call (the model command, its helpers) share 2 GiB, and when the last
        // handle to the job closes, everything in it is stopped.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.JobMemoryLimit = HARD_CAP_BYTES;

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

/// A Job Object that kills every process in it when the last handle closes (or on
/// `terminate`). Used for the model command: anything it starts, even a program that
/// outlives it (`start /B`), dies with the job.
#[cfg(windows)]
pub struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

// The handle is only used through `&self` calls that the OS serializes.
#[cfg(windows)]
unsafe impl Send for KillOnCloseJob {}

#[cfg(windows)]
impl KillOnCloseJob {
    pub fn new() -> Result<Self, String> {
        use std::mem::size_of;
        use std::ptr::null_mut;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let job = CreateJobObjectW(null_mut(), null_mut());
            if job.is_null() {
                return Err("CreateJobObjectW failed".into());
            }
            let guard = Self(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err("SetInformationJobObject failed".into());
            }
            Ok(guard)
        }
    }

    /// Puts a freshly started child (and so the processes it starts later) in the job.
    pub fn assign(&self, child: &std::process::Child) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) } == 0 {
            return Err(format!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Starts `command` already inside this job: the process is created suspended,
    /// assigned to the job and only then resumed, so it cannot start a grandchild that
    /// escapes the job in between. Returns the child and whether the assignment worked
    /// (if not, the child still runs, unconfined).
    pub fn spawn(&self, command: &mut std::process::Command) -> std::io::Result<(std::process::Child, bool)> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        #[link(name = "ntdll")]
        extern "system" {
            fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
        }
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_SUSPENDED);
        let child = command.spawn()?;
        let assigned = self.assign(&child).is_ok();
        // Resume even if the assignment failed: a suspended process would hang forever.
        let status = unsafe { NtResumeProcess(child.as_raw_handle() as _) };
        if status < 0 {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("could not resume the started process"));
        }
        Ok((child, assigned))
    }

    /// Kills every process in the job now.
    pub fn terminate(&self) {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1); }
    }
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); }
    }
}

/// Minutes the machine's local time is ahead of UTC right now (negative when behind), so
/// a time printed in local time can be turned into UTC. 0 where this is unknown.
pub fn local_utc_offset_minutes() -> i32 {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut tz);
        if id == 0xFFFF_FFFF {
            return 0;
        }
        let extra = match id {
            1 => tz.StandardBias,
            2 => tz.DaylightBias,
            _ => 0,
        };
        return -(tz.Bias + extra);
    }
    #[cfg(not(windows))]
    0
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::System::JobObjects::{QueryInformationJobObject, JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};

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
        assert_ne!(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY, 0, "job-wide, not per process");
        assert_ne!(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0);
        assert_eq!(info.JobMemoryLimit, HARD_CAP_BYTES);
    }

    /// The daemon's job: a program started after the job is attached is stopped when the
    /// job closes. Runs in a child test process because attaching moves the whole process.
    #[test]
    fn a_child_started_inside_the_job_dies_when_the_job_closes() {
        if std::env::var_os("MEMB_JOB_TEST_CHILD2").is_none() {
            return; // only meaningful as the child of `closing_the_job_leaves_no_running_child`
        }
        let guard = attach_job_object_2gb_cap().unwrap();
        let mut sleeper = std::process::Command::new("ping")
            .args(["-n", "41", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert!(sleeper.try_wait().unwrap().is_none(), "still running while the job is open");
        let pid = sleeper.id();
        // Closing the only handle kills every process in the job, including this test
        // process, so the check happens from the parent: it reads the pid we print.
        println!("SLEEPER-PID {pid}");
        drop(guard);
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    fn closing_the_job_leaves_no_running_child() {
        let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::a_child_started_inside_the_job_dies_when_the_job_closes", "--nocapture"])
            .env("MEMB_JOB_TEST_CHILD2", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = String::new();
        std::io::Read::read_to_string(parent.stdout.as_mut().unwrap(), &mut out).unwrap();
        let pid: u32 = out
            .lines()
            .find_map(|l| l.strip_prefix("SLEEPER-PID "))
            .expect("the child printed the sleeper's pid")
            .trim()
            .parse()
            .unwrap();
        let _ = parent.wait();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let listing = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&listing.stdout);
        assert!(!text.contains("ping.exe"), "the sleeper survived the job: {text}");
    }
}

#[cfg(test)]
mod governor_tests {
    use super::*;

    #[test]
    fn pressure_levels_gate_background_work_and_cache_trimming() {
        let at = |mib| classify_pressure(mib);
        assert_eq!(at(1199), PressureLevel::Normal);
        assert_eq!(at(1200), PressureLevel::Warn);
        assert_eq!(at(1549), PressureLevel::Warn);
        assert_eq!(at(1550), PressureLevel::TrimCaches);
        assert_eq!(at(1749), PressureLevel::TrimCaches);
        assert_eq!(at(1750), PressureLevel::ShedHeavyWork);
        // Background work runs until the shed threshold; caches are dropped from 1550 MiB.
        assert!(allow_background_jobs(at(1749)));
        assert!(!allow_background_jobs(at(1750)));
        assert!(!allow_background_jobs(at(5000)));
        assert!(!should_trim_caches(at(1549)));
        assert!(should_trim_caches(at(1550)));
        assert!(should_trim_caches(at(1800)));
    }

    #[test]
    fn the_tier_follows_the_cpu_report_and_sets_the_idle_timer() {
        let big = Hardware { avx512: false, avx2_fma: true, physical_cores: 8, total_ram_mib: 16 * 1024 };
        assert_eq!(choose_tier(&big), Tier::CpuBalanced);
        assert_eq!(Tier::CpuBalanced.idle_unload_secs(), 600);
        // Any one shortfall drops to Lite: cores, RAM, or no AVX2/AVX-512.
        assert_eq!(choose_tier(&Hardware { physical_cores: 7, ..big }), Tier::CpuLite);
        assert_eq!(choose_tier(&Hardware { total_ram_mib: 16 * 1024 - 1, ..big }), Tier::CpuLite);
        assert_eq!(choose_tier(&Hardware { avx2_fma: false, ..big }), Tier::CpuLite);
        assert_eq!(choose_tier(&Hardware { avx2_fma: false, avx512: true, ..big }), Tier::CpuBalanced);
        assert_eq!(Tier::CpuLite.idle_unload_secs(), 300);
        assert_eq!(choose_tier(&Hardware::default()), Tier::CpuLite);
    }

    #[cfg(windows)]
    #[test]
    fn the_prober_and_the_memory_reading_report_this_machine() {
        let hw = probe_hardware();
        assert!(hw.physical_cores >= 1 && hw.physical_cores <= 512, "{hw:?}");
        assert!(hw.total_ram_mib >= 512, "{hw:?}");
        let used = process_private_mib().expect("a memory reading");
        assert!(used >= 1 && used < 4096, "{used}");
    }
}
