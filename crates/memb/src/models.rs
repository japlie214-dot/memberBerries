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

/// Which ONNX build of the model. The 4-bit one is the small, fast one (Phase 2 `CpuLite`,
/// 256-wide vectors); the 8-bit one is larger and more precise (`CpuBalanced`, 512-wide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Q4,
    Int8,
}

impl Variant {
    pub const ALL: [Variant; 2] = [Variant::Q4, Variant::Int8];

    pub fn name(self) -> &'static str {
        match self {
            Variant::Q4 => "q4",
            Variant::Int8 => "int8",
        }
    }

    pub fn parse(text: &str) -> Option<Variant> {
        Variant::ALL.into_iter().find(|v| v.name() == text)
    }

    /// The vector width this variant is paired with (Phase 2 tiers).
    pub fn mrl_dim(self) -> usize {
        match self {
            Variant::Q4 => memb_win::Tier::CpuLite.mrl_dim(),
            Variant::Int8 => memb_win::Tier::CpuBalanced.mrl_dim(),
        }
    }

    pub fn manifest(self) -> Manifest {
        let file = |path: &str, size: u64, sha256: &str| ModelFile { path: path.into(), size, sha256: sha256.into() };
        let tokenizer = file("tokenizer.json", 32_170_510, "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4");
        let files = match self {
            Variant::Q4 => vec![
                tokenizer,
                file("onnx/model_q4.onnx", 490_742, "f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9"),
                file("onnx/model_q4.onnx_data", 174_028_800, "c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49"),
            ],
            Variant::Int8 => vec![
                tokenizer,
                file("onnx/model_quantized.onnx", 495_165, "d06edd601f851c633a2519304cbeb8dc6170d7ceb61b436625c17fb9b6e74953"),
                file("onnx/model_quantized.onnx_data", 313_724_928, "278a7ff1248c3618e4bd11a607fc54f7bdc7778854230f3956d3f86bd9db4f3b"),
            ],
        };
        Manifest { base_url: format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}"), files }
    }
}

/// The variant that fits this machine, or why none does (see `memb_win::plan_local`).
pub fn recommend(hw: &memb_win::Hardware) -> Result<Variant, &'static str> {
    match memb_win::plan_local(hw) {
        memb_win::LocalPlan::Local(memb_win::Tier::CpuLite) => Ok(Variant::Q4),
        memb_win::LocalPlan::Local(memb_win::Tier::CpuBalanced) => Ok(Variant::Int8),
        memb_win::LocalPlan::Unsupported(reason) => Err(reason),
    }
}

impl Manifest {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Where the model is kept: inside the data directory, so uninstalling the data removes it too.
/// One directory per variant; the 4-bit one keeps the plain name.
pub fn variant_dir(root: &Path, variant: Variant) -> PathBuf {
    match variant {
        Variant::Q4 => root.join("models").join("embeddinggemma-2"),
        Variant::Int8 => root.join("models").join("embeddinggemma-2-int8"),
    }
}

/// The complete installed variant memb should use: the one that fits this machine if it is
/// installed, otherwise any other complete one (for example a build the user chose to
/// install with `--variant` on a machine below the floor).
pub fn installed_variant(root: &Path, hw: &memb_win::Hardware) -> Option<(Variant, PathBuf)> {
    let preferred = recommend(hw).ok();
    let mut order: Vec<Variant> = preferred.into_iter().collect();
    order.extend(Variant::ALL.into_iter().filter(|v| Some(*v) != preferred));
    order.into_iter().find_map(|v| {
        let dir = variant_dir(root, v);
        is_complete(&dir, &v.manifest()).then_some((v, dir))
    })
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
    if let Some((variant, dir)) = installed_variant(root, &memb_win::probe_hardware()) {
        std::env::set_var("MEMB_GEMMA_MODEL_DIR", dir);
        // The vector width paired with the variant, unless the user chose one.
        if !set("MEMB_EMBED_MRL_DIM") {
            std::env::set_var("MEMB_EMBED_MRL_DIM", variant.mrl_dim().to_string());
        }
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

/// Which variant `install` should fetch: the one asked for with `--variant`, otherwise the
/// one that fits this machine. A machine below the floor gets an explanation, not a model
/// that cannot serve a search (the user can still ask for one with `--variant`).
pub fn plan_install(hw: &memb_win::Hardware, requested: Option<Variant>) -> Result<Variant, String> {
    match (requested, recommend(hw)) {
        (Some(v), _) => Ok(v),
        (None, Ok(v)) => Ok(v),
        (None, Err(reason)) => Err(format!(
            "this machine cannot run EmbeddingGemma 2 well: {reason}. Pass --variant q4 to install the smallest build anyway"
        )),
    }
}

fn describe(hw: &memb_win::Hardware) -> String {
    let plan = match memb_win::plan_local(hw) {
        memb_win::LocalPlan::Local(tier) => match recommend(hw) {
            Ok(v) => format!("tier {tier:?}: recommended build {} with {}-wide vectors", v.name(), v.mrl_dim()),
            Err(_) => format!("tier {tier:?}"),
        },
        memb_win::LocalPlan::Unsupported(reason) => format!("below the floor for a local model: {reason}"),
    };
    format!(
        "{} physical cores, {} MiB RAM, AVX-512 {}, AVX2+FMA {}; {plan}",
        hw.physical_cores, hw.total_ram_mib, hw.avx512, hw.avx2_fma
    )
}

/// `memb models status|install|remove`. Returns the exit code.
pub fn run(root: &Path, args: &[String]) -> i32 {
    let usage = "Usage: memb models status [--verify] | install [--variant q4|int8] [--yes] | remove [--yes]";
    let mut yes = false;
    let mut verify = false;
    let mut requested: Option<Variant> = None;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--yes" => yes = true,
            "--verify" => verify = true,
            "--variant" => match rest.next().and_then(|v| Variant::parse(v)) {
                Some(v) => requested = Some(v),
                None => {
                    eprintln!("memb: --variant takes q4 or int8\n{usage}");
                    return 2;
                }
            },
            _ => {
                eprintln!("{usage}");
                return 2;
            }
        }
    }
    let Some(command) = args.first() else {
        eprintln!("{usage}");
        return 2;
    };
    let hw = memb_win::probe_hardware();
    match command.as_str() {
        "status" => {
            println!("EmbeddingGemma 2 (ONNX), revision {REVISION}");
            println!("this machine: {}", describe(&hw));
            for variant in Variant::ALL {
                let (dir, manifest) = (variant_dir(root, variant), variant.manifest());
                println!("{} ({:.0} MB, {}-wide vectors): {}", variant.name(), manifest.total_bytes() as f64 / 1e6, variant.mrl_dim(), dir.display());
                for (path, state) in inspect(&dir, &manifest, verify) {
                    let text = match state {
                        FileState::Missing => "missing".to_string(),
                        FileState::WrongSize(n) => format!("wrong size ({n} bytes)"),
                        FileState::Present => "present (size matches; use --verify to check the content)".to_string(),
                        FileState::Verified => "verified (SHA-256 matches)".to_string(),
                        FileState::Corrupt => "CORRUPT (SHA-256 does not match)".to_string(),
                    };
                    println!("  {path}: {text}");
                }
            }
            match installed_variant(root, &hw) {
                Some((v, dir)) => {
                    // The loader does not hash files, so a corrupt file must not be called ready.
                    if inspect(&dir, &v.manifest(), verify).iter().any(|(_, s)| *s == FileState::Corrupt) {
                        println!("NOT ready: the {} build in {} has a corrupt file; run `memb models remove` and `memb models install`", v.name(), dir.display());
                        return 1;
                    }
                    println!("ready: memb uses the {} build in {} automatically unless you chose another backend", v.name(), dir.display());
                    0
                }
                None => {
                    match recommend(&hw) {
                        Ok(v) => println!("not installed: run `memb models install` (fetches the {} build, {:.0} MB)", v.name(), v.manifest().total_bytes() as f64 / 1e6),
                        Err(reason) => println!("not installed, and this machine is below the floor for a local model ({reason})"),
                    }
                    1
                }
            }
        }
        "install" => {
            let variant = match plan_install(&hw, requested) {
                Ok(v) => v,
                Err(reason) => {
                    eprintln!("memb: {reason}");
                    return 1;
                }
            };
            let (dir, manifest) = (variant_dir(root, variant), variant.manifest());
            println!(
                "memb will download the {} build: {} files ({:.0} MB) from {}\nto {}\nEvery file is checked against a pinned size and SHA-256. Nothing else is downloaded.",
                variant.name(),
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
                    println!("done: {n} file(s) downloaded; memb now uses the model automatically with {}-wide vectors (restart a running memb server or host to pick it up)", variant.mrl_dim());
                    0
                }
                Err(reason) => {
                    eprintln!("memb: {reason}");
                    1
                }
            }
        }
        "remove" => {
            let present: Vec<PathBuf> = Variant::ALL.into_iter().map(|v| variant_dir(root, v)).filter(|d| d.exists()).collect();
            if present.is_empty() {
                println!("nothing to remove: no model directory exists under {}", root.join("models").display());
                return 0;
            }
            let list = present.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
            if !yes && !confirm(&format!("Type yes to delete {list}: ")) {
                eprintln!("memb: not removed (pass --yes to confirm without a prompt)");
                return 1;
            }
            for dir in &present {
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    eprintln!("memb: cannot remove {}: {e}", dir.display());
                    return 1;
                }
            }
            println!("removed {list}; search is lexical again unless another backend is configured");
            0
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
        let m = Variant::Q4.manifest();
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

    fn hw(avx2: bool, avx512: bool, cores: usize, ram_mib: usize) -> memb_win::Hardware {
        memb_win::Hardware { avx512, avx2_fma: avx2, physical_cores: cores, total_ram_mib: ram_mib }
    }

    #[test]
    fn the_variant_follows_the_machine_and_a_machine_below_the_floor_gets_an_explanation() {
        // (hardware, expected build or reason fragment)
        let table = [
            (hw(true, false, 12, 16 * 1024), Ok(Variant::Int8)),   // 8+ cores, 16 GB, AVX2: balanced
            (hw(false, true, 8, 32 * 1024), Ok(Variant::Int8)),    // AVX-512 only, still balanced
            (hw(true, false, 12, 15 * 1024), Ok(Variant::Q4)),     // under 16 GB: lite
            (hw(true, false, 4, 32 * 1024), Ok(Variant::Q4)),      // under 8 cores: lite
            (hw(true, false, 4, 8 * 1024), Ok(Variant::Q4)),       // exactly 8 GB: lite, still allowed
            (hw(true, false, 16, 8 * 1024 - 1), Err("RAM")),       // under 8 GB: below the floor
            (hw(false, false, 16, 32 * 1024), Err("AVX2")),        // no AVX2/FMA or AVX-512: below the floor
        ];
        for (hardware, expected) in table {
            match (recommend(&hardware), expected) {
                (Ok(got), Ok(want)) => assert_eq!(got, want, "{hardware:?}"),
                (Err(reason), Err(fragment)) => assert!(reason.contains(fragment), "{reason}"),
                (got, want) => panic!("{hardware:?}: got {got:?}, wanted {want:?}"),
            }
        }
        // Each build is paired with its Phase 2 vector width, and they differ.
        assert_eq!((Variant::Q4.mrl_dim(), Variant::Int8.mrl_dim()), (256, 512));
        // install without --variant refuses below the floor and says how to override; with it, it proceeds.
        let weak = hw(false, false, 2, 4 * 1024);
        let err = plan_install(&weak, None).unwrap_err();
        assert!(err.contains("cannot run EmbeddingGemma 2 well") && err.contains("--variant q4"), "{err}");
        assert_eq!(plan_install(&weak, Some(Variant::Q4)).unwrap(), Variant::Q4);
        assert_eq!(plan_install(&hw(true, false, 12, 16 * 1024), Some(Variant::Q4)).unwrap(), Variant::Q4, "the user's choice wins");
        assert_eq!(plan_install(&hw(true, false, 12, 16 * 1024), None).unwrap(), Variant::Int8);
        assert_eq!(Variant::parse("int8"), Some(Variant::Int8));
        assert_eq!(Variant::parse("q8"), None);
    }

    #[test]
    fn status_is_not_ready_when_verification_finds_a_corrupt_file_and_arguments_are_checked() {
        let _guard = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp("status");
        // Right sizes, wrong content: present by size, corrupt by hash.
        let v = Variant::Q4;
        for f in &v.manifest().files {
            let p = file_path(&variant_dir(&root, v), f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::File::create(&p).unwrap().set_len(f.size).unwrap();
        }
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Only the 4-bit build exists, so this is the build status reports on any hardware.
        assert_eq!(run(&root, &args(&["status"])), 0, "sizes match: ready without hashing");
        assert_eq!(run(&root, &args(&["status", "--verify"])), 1, "the content does not match the pinned SHA-256");
        // Argument handling: unknown flags, a missing or unknown --variant value, no command.
        assert_eq!(run(&root, &args(&["status", "--bogus"])), 2);
        assert_eq!(run(&root, &args(&["install", "--variant"])), 2);
        assert_eq!(run(&root, &args(&["install", "--variant", "q8"])), 2);
        assert_eq!(run(&root, &args(&[])), 2);
        assert_eq!(run(&root, &args(&["frobnicate"])), 2);
        // remove deletes the directory only with --yes (a test has no terminal to type in).
        assert_eq!(run(&root, &args(&["remove"])), 1);
        assert!(variant_dir(&root, v).exists());
        assert_eq!(run(&root, &args(&["remove", "--yes"])), 0);
        assert!(!variant_dir(&root, v).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_pinned_int8_manifest_and_the_installed_variant_choice() {
        let m = Variant::Int8.manifest();
        let paths: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["tokenizer.json", "onnx/model_quantized.onnx", "onnx/model_quantized.onnx_data"]);
        assert_eq!(m.total_bytes(), 346_390_603);
        assert!(m.files.iter().all(|f| f.sha256.len() == 64));
        assert_ne!(variant_dir(Path::new("r"), Variant::Q4), variant_dir(Path::new("r"), Variant::Int8));
        // A balanced machine prefers int8 when both are present, falls back to q4 when only that is,
        // and a below-floor machine still uses what the user chose to install.
        let root = temp("variants");
        let (balanced, weak) = (hw(true, false, 12, 16 * 1024), hw(false, false, 2, 4 * 1024));
        assert!(installed_variant(&root, &balanced).is_none());
        let fill = |v: Variant| {
            for f in &v.manifest().files {
                let p = file_path(&variant_dir(&root, v), f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::File::create(&p).unwrap().set_len(f.size).unwrap();
            }
        };
        fill(Variant::Q4);
        assert_eq!(installed_variant(&root, &balanced).unwrap().0, Variant::Q4);
        assert_eq!(installed_variant(&root, &weak).unwrap().0, Variant::Q4);
        fill(Variant::Int8);
        assert_eq!(installed_variant(&root, &balanced).unwrap().0, Variant::Int8);
        assert_eq!(installed_variant(&root, &hw(true, false, 4, 8 * 1024)).unwrap().0, Variant::Q4, "a lite machine prefers q4");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_complete_default_directory_is_used_unless_a_backend_was_chosen() {
        let _guard = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = Variant::Q4.manifest();
        let root = temp("default");
        let dir = variant_dir(&root, Variant::Q4);
        assert!(dir.ends_with("models\\embeddinggemma-2") || dir.ends_with("models/embeddinggemma-2"));
        let saved: Vec<_> = ["MEMB_GEMMA_MODEL_DIR", "MEMB_EMBED_PORT", "MEMB_EMBED_DISABLE", "MEMB_EMBED_MRL_DIM"].iter().map(|k| (*k, std::env::var_os(k))).collect();
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
        assert_eq!(std::env::var("MEMB_EMBED_MRL_DIM").unwrap(), "256", "the width paired with the 4-bit build");
        std::env::remove_var("MEMB_GEMMA_MODEL_DIR");
        std::env::set_var("MEMB_EMBED_MRL_DIM", "512");
        apply_default(&root);
        assert_eq!(std::env::var("MEMB_EMBED_MRL_DIM").unwrap(), "512", "a width the user chose is kept");
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
