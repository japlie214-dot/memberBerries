//! Per-user named pipes: a local channel between memb processes with no network port.
//!
//! The pipe is created with an access list that names only the current user's SID (so
//! another account on the machine cannot connect, whatever the default would allow), and it
//! refuses remote clients. The first process to create a pipe name owns it; a second one
//! asking for the same name fails instead of sharing it.
use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `\\.\pipe\<name>`. The name is limited to letters, digits, `-` and `_`, so it cannot
/// point anywhere else in the pipe namespace.
pub fn pipe_path(name: &str) -> io::Result<String> {
    if name.is_empty() || name.len() > 100 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "pipe names hold only letters, digits, - and _"));
    }
    Ok(format!(r"\\.\pipe\{name}"))
}

/// The current user's SID as the text SDDL uses (`S-1-5-21-...`).
fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut needed = 0u32;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0u8; needed as usize];
            if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
            let mut text: *mut u16 = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(sid)
        })();
        CloseHandle(token);
        result
    }
}

pub struct PipeListener {
    path: Vec<u16>,
    sddl: Vec<u16>,
    pending: HANDLE,
}

// The pending handle is only touched through &mut self.
unsafe impl Send for PipeListener {}

fn create_instance(path: &[u16], sddl: &[u16], first: bool) -> io::Result<HANDLE> {
    unsafe {
        let mut descriptor = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = CreateNamedPipeW(
            path.as_ptr(),
            PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        );
        let error = io::Error::last_os_error();
        LocalFree(descriptor);
        if handle == INVALID_HANDLE_VALUE {
            Err(error)
        } else {
            Ok(handle)
        }
    }
}

impl PipeListener {
    /// Creates the pipe. Fails if another process already owns this name.
    pub fn bind(name: &str) -> io::Result<Self> {
        let path = wide(&pipe_path(name)?);
        // Owner access only: the current user's SID, nobody else, protected from inheritance.
        let sddl = wide(&format!("D:P(A;;GA;;;{})", current_user_sid()?));
        let pending = create_instance(&path, &sddl, true)?;
        Ok(Self { path, sddl, pending })
    }

    /// Waits for the next client and returns its end of the connection. Each call hands out
    /// one pipe instance, so clients are served concurrently if the caller uses threads.
    pub fn accept(&mut self) -> io::Result<File> {
        unsafe {
            if ConnectNamedPipe(self.pending, null_mut()) == 0 {
                let error = GetLastError();
                if error != ERROR_PIPE_CONNECTED {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
            }
        }
        let next = create_instance(&self.path, &self.sddl, false);
        let connected = std::mem::replace(&mut self.pending, *next.as_ref().unwrap_or(&null_mut()));
        match next {
            Ok(_) => Ok(unsafe { File::from_raw_handle(connected as _) }),
            Err(e) => {
                unsafe { CloseHandle(connected) };
                self.pending = null_mut();
                Err(e)
            }
        }
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        if !self.pending.is_null() {
            unsafe { CloseHandle(self.pending) };
        }
        let _ = null::<u8>();
    }
}

/// Opens the client end. `NotFound` means no process owns the pipe (the service is not
/// running). A busy pipe is retried for about a second.
pub fn connect(name: &str) -> io::Result<File> {
    let path = pipe_path(name)?;
    let mut last = None;
    for _ in 0..50 {
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => return Ok(file),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "pipe busy")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn unique(tag: &str) -> String {
        format!("memb-test-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }

    #[test]
    fn a_client_and_a_server_talk_and_a_second_owner_of_the_name_is_refused() {
        let name = unique("echo");
        assert!(connect(&name).unwrap_err().kind() == io::ErrorKind::NotFound, "nobody owns it yet");
        let mut listener = PipeListener::bind(&name).unwrap();
        assert!(PipeListener::bind(&name).is_err(), "the first instance is exclusive to its creator");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let mut stream = listener.accept().unwrap();
                let mut buf = [0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                stream.write_all(&buf.map(|b| b.to_ascii_uppercase())).unwrap();
            }
        });
        for word in [b"hello", b"world"] {
            let mut client = connect(&name).unwrap();
            client.write_all(word).unwrap();
            let mut answer = [0u8; 5];
            client.read_exact(&mut answer).unwrap();
            assert_eq!(&answer, &word.map(|b| b.to_ascii_uppercase()));
        }
        server.join().unwrap();
    }

    #[test]
    fn names_that_could_reach_other_pipes_are_refused() {
        for bad in ["", "a\\b", "..", "x/y", "a b", &"n".repeat(101)] {
            assert!(pipe_path(bad).is_err(), "{bad}");
        }
        assert_eq!(pipe_path("memb-embed_1").unwrap(), r"\\.\pipe\memb-embed_1");
    }

    #[test]
    fn the_security_descriptor_names_only_the_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-") && sid.len() > 10, "{sid}");
    }
}
