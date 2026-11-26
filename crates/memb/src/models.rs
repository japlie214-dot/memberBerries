//! The EmbeddingGemma 2 model: where it lives, how it is fetched and how it is verified.
//!
//! memb does not ship the model (about 207 MB). `memb models install` downloads the three
//! files of one pinned revision from Hugging Face into `<data dir>\models\embeddinggemma-2`,
//! checks every file's size and SHA-256 against the values pinned here, and only then makes
//! them visible. Once the directory is complete, memb uses it without any configuration.
//!
//! The download is done by Windows' own `curl.exe` (resumable, follows redirects, no extra
//! dependency in memb). Nothing is downloaded without the user's consent: `install` asks, or
//! needs `--yes`; the daily routine downloads only when `embeddings.auto_download = true`.
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const REPOSITORY: &str = "onnx-community/embeddinggemma-2-ONNX";
pub const REVISION: &str = "daa72c51243991dfcaf9f9137d2c573d8f7790c0";

#[derive(Debug, Clone)]
pub struct ModelFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub base_url: String,
    pub files: Vec<ModelFile>,
}

impl Manifest {
    /// The pinned revision. Sizes and hashes come from the repository's file listing at
    /// that revision; the in-process loader (`memb-embed`) needs exactly these three files.
    pub fn embeddinggemma() -> Self {
        let file = |path: &str, size: u64, sha256: &str| ModelFile { path: path.into(), size, sha256: sha256.into() };
        Self {
            base_url: format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}"),
            files: vec![
                file("tokenizer.json", 32_170_510, "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4"),
                file("onnx/model_q4.onnx", 490_742, "f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9"),
                file("onnx/model_q4.onnx_data", 174_028_800, "c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49"),
            ],
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Where the model is kept: inside the data directory, so uninstalling the data removes it too.
pub fn default_dir(root: &Path) -> PathBuf {
    root.join("models").join("embeddinggemma-2")
}

fn file_path(dir: &Path, file: &ModelFile) -> PathBuf {
    file.path.split('/').fold(dir.to_path_buf(), |p, part| p.join(part))
}

/// Every file exists with exactly the pinned size (a cheap check; `verify` also hashes).
pub fn is_complete(dir: &Path, manifest: &Manifest) -> bool {
    manifest
        .files
        .iter()
        .all(|f| std::fs::metadata(file_path(dir, f)).is_ok_and(|m| m.is_file() && m.len() == f.size))
}

/// When no backend was chosen (no `MEMB_GEMMA_MODEL_DIR`, no loopback port, not disabled) and
/// the default directory holds a complete model, use it. An explicit choice always wins.
pub fn apply_default(root: &Path) {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if std::env::var("MEMB_EMBED_DISABLE").ok().as_deref() == Some("1")
        || set("MEMB_GEMMA_MODEL_DIR")
        || set("MEMB_EMBED_PORT")
    {
        return;
    }
    let dir = default_dir(root);
    if is_complete(&dir, &Manifest::embeddinggemma()) {
        std::env::set_var("MEMB_GEMMA_MODEL_DIR", dir);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FileState {
    Missing,
    WrongSize(u64),
    /// The size matches; the content was not hashed.
    Present,
    Verified,
    Corrupt,
}

pub fn inspect(dir: &Path, manifest: &Manifest, hash: bool) -> Vec<(String, FileState)> {
    manifest
        .files
        .iter()
        .map(|f| {
            let path = file_path(dir, f);
            let state = match std::fs::metadata(&path) {
                Err(_) => FileState::Missing,
                Ok(m) if !m.is_file() => FileState::Missing,
                Ok(m) if m.len() != f.size => FileState::WrongSize(m.len()),
                Ok(_) if !hash => FileState::Present,
                Ok(_) => match memb_embed::sha256_file(&path) {
                    Ok(h) if h == f.sha256 => FileState::Verified,
                    _ => FileState::Corrupt,
                },
            };
            (f.path.clone(), state)
        })
        .collect()
}

fn curl_exe() -> Result<PathBuf, String> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let curl = root.join("System32").join("curl.exe");
    if curl.is_file() {
        Ok(curl)
    } else {
        Err(format!(
            "Windows' curl.exe was not found at {}; download the files listed by `memb models status` into the model directory yourself",
            curl.display()
        ))
    }
}

fn download(curl: &Path, url: &str, part: &Path, progress: bool) -> Result<(), String> {
    let scheme = if url.starts_with("https://") { "=https" } else { "=http" };
    let mut cmd = Command::new(curl);
    cmd.args(["--fail", "--location", "--show-error", "--retry", "3", "--connect-timeout", "30"])
        .args(["--proto", scheme, "--proto-redir", "=https,http", "-C", "-"])
        .arg(if progress { "--progress-bar" } else { "--silent" })
        .arg("--output")
        .arg(part)
        .arg(url)
        .stdin(Stdio::null());
    let status = cmd.status().map_err(|e| format!("could not start curl.exe: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("download failed (curl exit {}); run the command again to resume", status.code().unwrap_or(-1)))
    }
}

/// Downloads and verifies every file that is not already correct. A file becomes visible
/// under its final name only after its size and SHA-256 match; a failed or interrupted
/// download leaves a `.part` file that the next run resumes. Returns how many were fetched.
pub fn install(dir: &Path, manifest: &Manifest, progress: bool, out: &mut dyn Write) -> Result<usize, String> {
    let curl = curl_exe()?;
    let mut fetched = 0;
    for file in &manifest.files {
        let dest = file_path(dir, file);
        if inspect(dir, &Manifest { base_url: String::new(), files: vec![file.clone()] }, true)[0].1 == FileState::Verified {
            let _ = writeln!(out, "ok       {} (already present)", file.path);
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let part = PathBuf::from(format!("{}.part", dest.display()));
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if have > file.size {
            let _ = std::fs::remove_file(&part);
        }
        let _ = writeln!(out, "fetching {} ({:.1} MB)", file.path, file.size as f64 / 1e6);
        let _ = out.flush();
        let url = format!("{}/{}", manifest.base_url.trim_end_matches('/'), file.path);
        if std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) != file.size {
            download(&curl, &url, &part, progress)?;
        }
        let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let hash = memb_embed::sha256_file(&part).unwrap_or_default();
        if size != file.size || hash != file.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "{} did not match the pinned size and SHA-256 ({size} bytes); the partial file was removed, nothing was installed for it",
                file.path
            ));
        }
        let _ = std::fs::remove_file(&dest);
        std::fs::rename(&part, &dest).map_err(|e| format!("cannot move {} into place: {e}", file.path))?;
        fetched += 1;
    }
    Ok(fetched)
}

fn confirm(prompt: &str) -> bool {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return false;
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("yes")
}

/// `memb models status|install|remove`. Returns the exit code.
pub fn run(root: &Path, args: &[String]) -> i32 {
    let manifest = Manifest::embeddinggemma();
    let dir = default_dir(root);
    let usage = "Usage: memb models status [--verify] | install [--yes] | remove [--yes]";
    let yes = args.iter().any(|a| a == "--yes");
    let verify = args.iter().any(|a| a == "--verify");
    let known = |flag: &str| matches!(flag, "--yes" | "--verify");
    if args.is_empty() || args[1..].iter().any(|a| !known(a)) {
        eprintln!("{usage}");
        return 2;
    }
    match args[0].as_str() {
        "status" => {
            println!("EmbeddingGemma 2 (ONNX q4), revision {REVISION}");
            println!("directory: {}", dir.display());
            let states = inspect(&dir, &manifest, verify);
            for (path, state) in &states {
                let text = match state {
                    FileState::Missing => "missing".to_string(),
                    FileState::WrongSize(n) => format!("wrong size ({n} bytes)"),
                    FileState::Present => "present (size matches; use --verify to check the content)".to_string(),
                    FileState::Verified => "verified (SHA-256 matches)".to_string(),
                    FileState::Corrupt => "CORRUPT (SHA-256 does not match)".to_string(),
                };
                println!("  {path}: {text}");
            }
            if is_complete(&dir, &manifest) && !states.iter().any(|(_, s)| *s == FileState::Corrupt) {
                println!("ready: memb uses this model automatically unless you chose another backend");
                0
            } else {
                println!("not installed: run `memb models install` ({:.0} MB)", manifest.total_bytes() as f64 / 1e6);
                1
            }
        }
        "install" => {
            println!(
                "memb will download {} files ({:.0} MB) from {}\nto {}\nEvery file is checked against a pinned size and SHA-256. Nothing else is downloaded.",
                manifest.files.len(),
                manifest.total_bytes() as f64 / 1e6,
                manifest.base_url,
                dir.display()
            );
            if !yes && !confirm("Type yes to download: ") {
                eprintln!("memb: not downloaded (pass --yes to confirm without a prompt)");
                return 1;
            }
            match install(&dir, &manifest, std::io::stderr().is_terminal(), &mut std::io::stdout()) {
                Ok(n) => {
                    println!("done: {n} file(s) downloaded; memb now uses the model automatically (restart a running memb server or host to pick it up)");
                    0
                }
                Err(reason) => {
                    eprintln!("memb: {reason}");
                    1
                }
            }
        }
        "remove" => {
            if !dir.exists() {
                println!("nothing to remove: {} does not exist", dir.display());
                return 0;
            }
            if !yes && !confirm(&format!("Type yes to delete {}: ", dir.display())) {
                eprintln!("memb: not removed (pass --yes to confirm without a prompt)");
                return 1;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    println!("removed {}; search is lexical again unless another backend is configured", dir.display());
                    0
                }
                Err(e) => {
                    eprintln!("memb: cannot remove {}: {e}", dir.display());
                    1
                }
            }
        }
        _ => {
            eprintln!("{usage}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memb-models-{name}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha(bytes: &[u8]) -> String {
        let dir = temp("sha");
        let p = dir.join("f");
        std::fs::write(&p, bytes).unwrap();
        let h = memb_embed::sha256_file(&p).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        h
    }

    /// A loopback file server that honours `Range`, and records every request.
    fn serve(files: Vec<(String, Vec<u8>)>) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                let range = text
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(|r| r.trim_end_matches('-').to_string()))
                    .and_then(|r| r.parse::<usize>().ok());
                seen.lock().unwrap().push(format!("{path} range={range:?}"));
                let Some((_, body)) = files.iter().find(|(p, _)| path.ends_with(p.as_str())) else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                };
                let (status, slice, extra) = match range {
                    Some(from) if from < body.len() => (
                        "206 Partial Content",
                        &body[from..],
                        format!("Content-Range: bytes {from}-{}/{}\r\n", body.len() - 1, body.len()),
                    ),
                    _ => ("200 OK", &body[..], String::new()),
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", slice.len());
                let _ = stream.write_all(slice);
            }
        });
        (port, log)
    }

    fn manifest(port: u16, a: &[u8], b: &[u8]) -> Manifest {
        Manifest {
            base_url: format!("http://127.0.0.1:{port}/repo"),
            files: vec![
                ModelFile { path: "tokenizer.json".into(), size: a.len() as u64, sha256: sha(a) },
                ModelFile { path: "onnx/model.bin".into(), size: b.len() as u64, sha256: sha(b) },
            ],
        }
    }

    #[test]
    fn the_pinned_manifest_names_exactly_the_files_the_loader_needs() {
        let m = Manifest::embeddinggemma();
        let paths: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["tokenizer.json", "onnx/model_q4.onnx", "onnx/model_q4.onnx_data"]);
        assert!(m.base_url.starts_with("https://huggingface.co/") && m.base_url.ends_with(REVISION));
        assert!(m.files.iter().all(|f| f.sha256.len() == 64 && f.size > 0));
        assert_eq!(m.total_bytes(), 206_690_052);
    }

    #[test]
    fn install_downloads_verifies_skips_what_is_present_and_resumes_a_partial_file() {
        let a: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..70_000u32).map(|i| (i % 241) as u8).collect();
        let (port, log) = serve(vec![("/repo/tokenizer.json".into(), a.clone()), ("/repo/onnx/model.bin".into(), b.clone())]);
        let m = manifest(port, &a, &b);
        let dir = temp("install");
        assert!(!is_complete(&dir, &m));
        // A partial download from an earlier, interrupted run: the second half is requested only.
        std::fs::create_dir_all(dir.join("onnx")).unwrap();
        std::fs::write(dir.join("onnx").join("model.bin.part"), &b[..30_000]).unwrap();
        let mut out = Vec::new();
        assert_eq!(install(&dir, &m, false, &mut out).unwrap(), 2, "{}", String::from_utf8_lossy(&out));
        assert!(is_complete(&dir, &m));
        assert_eq!(std::fs::read(dir.join("onnx").join("model.bin")).unwrap(), b);
        assert!(!dir.join("onnx").join("model.bin.part").exists());
        assert!(inspect(&dir, &m, true).iter().all(|(_, s)| *s == FileState::Verified));
        let requests = log.lock().unwrap().clone();
        assert!(requests.iter().any(|r| r.contains("model.bin") && r.contains("range=Some(30000)")), "{requests:?}");
        // A second run changes nothing and asks the server for nothing.
        let before = log.lock().unwrap().len();
        let mut out = Vec::new();
        assert_eq!(install(&dir, &m, false, &mut out).unwrap(), 0);
        assert!(String::from_utf8_lossy(&out).contains("already present"));
        assert_eq!(log.lock().unwrap().len(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_with_the_wrong_content_is_never_installed_and_a_damaged_file_is_replaced() {
        let a = b"the real tokenizer".to_vec();
        let b = b"the real model".to_vec();
        // The server serves a file of the right size with different content.
        let bad = vec![b'x'; b.len()];
        let (port, _) = serve(vec![("/repo/tokenizer.json".into(), a.clone()), ("/repo/onnx/model.bin".into(), bad)]);
        let m = manifest(port, &a, &b);
        let dir = temp("tamper");
        let err = install(&dir, &m, false, &mut Vec::new()).unwrap_err();
        assert!(err.contains("onnx/model.bin") && err.contains("SHA-256"), "{err}");
        assert!(!dir.join("onnx").join("model.bin").exists(), "a file that fails verification is never visible");
        assert!(!dir.join("onnx").join("model.bin.part").exists(), "the bad partial file is removed");
        assert!(!is_complete(&dir, &m));
        // The first file was good and stays; a damaged copy of it is detected and replaced.
        assert_eq!(inspect(&dir, &m, true)[0].1, FileState::Verified);
        std::fs::write(dir.join("tokenizer.json"), b"the fake tokenizer").unwrap();
        assert_eq!(inspect(&dir, &m, false)[0].1, FileState::Present, "same size, so only hashing finds it");
        assert_eq!(inspect(&dir, &m, true)[0].1, FileState::Corrupt);
        let (port, _) = serve(vec![("/repo/tokenizer.json".into(), a.clone()), ("/repo/onnx/model.bin".into(), b.clone())]);
        let m = manifest(port, &a, &b);
        assert_eq!(install(&dir, &m, false, &mut Vec::new()).unwrap(), 2);
        assert!(inspect(&dir, &m, true).iter().all(|(_, s)| *s == FileState::Verified));
        // A missing file on the server fails the install with a resumable message.
        let (port, _) = serve(vec![]);
        let m = manifest(port, &a, &b);
        let dir2 = temp("missing");
        assert!(install(&dir2, &m, false, &mut Vec::new()).is_err());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn a_complete_default_directory_is_used_unless_a_backend_was_chosen() {
        let _guard = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = Manifest::embeddinggemma();
        let root = temp("default");
        let dir = default_dir(&root);
        assert!(dir.ends_with("models\\embeddinggemma-2") || dir.ends_with("models/embeddinggemma-2"));
        let saved: Vec<_> = ["MEMB_GEMMA_MODEL_DIR", "MEMB_EMBED_PORT", "MEMB_EMBED_DISABLE"].iter().map(|k| (*k, std::env::var_os(k))).collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        // Incomplete: nothing is set.
        apply_default(&root);
        assert!(std::env::var_os("MEMB_GEMMA_MODEL_DIR").is_none());
        // Complete (right sizes, content irrelevant for this check): used.
        for f in &m.files {
            let p = file_path(&dir, f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let file = std::fs::File::create(&p).unwrap();
            file.set_len(f.size).unwrap();
        }
        assert!(is_complete(&dir, &m));
        // An explicit port, or disabling, wins.
        std::env::set_var("MEMB_EMBED_PORT", "1234");
        apply_default(&root);
        assert!(std::env::var_os("MEMB_GEMMA_MODEL_DIR").is_none());
        std::env::remove_var("MEMB_EMBED_PORT");
        std::env::set_var("MEMB_EMBED_DISABLE", "1");
        apply_default(&root);
        assert!(std::env::var_os("MEMB_GEMMA_MODEL_DIR").is_none());
        std::env::remove_var("MEMB_EMBED_DISABLE");
        apply_default(&root);
        assert_eq!(std::env::var_os("MEMB_GEMMA_MODEL_DIR").map(PathBuf::from), Some(dir.clone()));
        // A size that is off by one makes it incomplete again.
        std::fs::File::options().write(true).open(file_path(&dir, &m.files[1])).unwrap().set_len(m.files[1].size - 1).unwrap();
        assert!(!is_complete(&dir, &m));
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
