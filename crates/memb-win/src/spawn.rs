//! Starting a process that is fully detached from the caller.
//!
//! `std::process::Command` always lets the child inherit every inheritable handle of the
//! parent, including the pipes a host or a shell uses to capture this process's output. A
//! long-lived child (the shared embedding service) that holds such a pipe keeps the caller
//! waiting for end-of-output until the child exits. Here nothing is inherited: no standard
//! handles, no pipes.
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    PROCESS_INFORMATION, STARTUPINFOW,
};

const BS: u16 = 0x5c;

fn quote(arg: &OsStr) -> Vec<u16> {
    let text: Vec<u16> = arg.encode_wide().collect();
    let needs = text.is_empty() || text.iter().any(|c| *c == b' ' as u16 || *c == b'\t' as u16 || *c == b'"' as u16);
    if !needs {
        return text;
    }
    let mut out = vec![b'"' as u16];
    let mut backslashes = 0usize;
    for c in text {
        if c == BS {
            backslashes += 1;
            continue;
        }
        if c == b'"' as u16 {
            out.extend(std::iter::repeat(BS).take(backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat(BS).take(backslashes));
        }
        backslashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat(BS).take(backslashes * 2));
    out.push(b'"' as u16);
    out
}

/// Starts `exe args...` detached: no console, its own process group, no inherited handles.
/// `breakaway` asks to leave the caller's job object (refused if the job forbids it).
pub fn spawn_detached(exe: &Path, args: &[&str], breakaway: bool) -> io::Result<()> {
    let mut line: Vec<u16> = quote(exe.as_os_str());
    for a in args {
        line.push(b' ' as u16);
        line.extend(quote(OsStr::new(a)));
    }
    line.push(0);
    let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    unsafe {
        let startup: STARTUPINFOW = {
            let mut s: STARTUPINFOW = std::mem::zeroed();
            s.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            s
        };
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        // bInheritHandles = FALSE is the point of this function.
        let ok = CreateProcessW(null(), line.as_mut_ptr(), null(), null(), 0, flags, null_mut(), null(), &startup, &mut info);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_the_windows_command_line() {
        let q = |s: &str| String::from_utf16(&quote(OsStr::new(s))).unwrap();
        assert_eq!(q("plain"), "plain");
        assert_eq!(q("a b"), "\"a b\"");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\Program Files\x\"), r#""C:\Program Files\x\\""#);
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        assert!(spawn_detached(Path::new(r"C:\no\such\memb-xyz.exe"), &["embed-service"], false).is_err());
    }
}
