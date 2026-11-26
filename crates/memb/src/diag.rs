//! Read-only diagnostics: `memb doctor` and `memb inspect session`.
//!
//! Nothing here prints note text or conversation text (only `memb chunk read`, which is
//! gated like the `scroll_chunk_read` tool, does), and nothing is sent to any model: the
//! health check only looks at files, ports and the process table.
use crate::config::Config;
use memb_store::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Session keys are stored as `<cli>_<id>`; accept either form.
pub fn inspect_session(store: &Store, cfg: &Config, wanted: &str) -> Result<Vec<Value>, String> {
    if wanted.is_empty()
        || wanted.len() > 160
        || !wanted.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("invalid session id".into());
    }
    let keys: Vec<String> = if ["cc_", "codex_", "agy_"].iter().any(|p| wanted.starts_with(p)) {
        vec![wanted.to_owned()]
    } else {
        ["cc", "codex", "agy"].iter().map(|cli| format!("{cli}_{wanted}")).collect()
    };
    let reset = cfg.scroll.dedup_reset_every_n_chunks;
    let mut found = Vec::new();
    for key in keys {
        if let Some(s) = store.with_db(|db| memb_scroll::session_state(db, &key, reset))? {
            found.push(json!({
                "session_key": key,
                "cli": s.cli,
                "messages": s.messages,
                "max_messages": cfg.scroll.max_messages,
                "total_chunks": s.total_chunks,
                "dedup_epoch": s.dedup_epoch,
                "dedup_reset_every_n_chunks": reset,
                "injected_note_ids": s.injected_note_ids,
                "last_active_ms": s.last_active_ms,
                "memb_process_private_mib": memb_win::process_private_mib(),
            }));
        }
    }
    Ok(found)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self { name, status, detail: detail.into() }
    }
    pub fn line(&self) -> String {
        let tag = match self.status {
            Status::Ok => "ok  ",
            Status::Info => "info",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        };
        format!("{tag} {}: {}", self.name, self.detail)
    }
}

/// Finds a program the way a command line would: an explicit path, or each `PATH`
/// directory with each `PATHEXT` extension. Nothing is run.
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_lowercase)
        .collect();
    let try_in = |dir: &Path| -> Option<PathBuf> {
        let base = dir.join(program);
        if base.is_file() {
            return Some(base);
        }
        exts.iter().map(|e| dir.join(format!("{program}{e}"))).find(|p| p.is_file())
    };
    if program.contains(['\\', '/']) {
        return try_in(Path::new(""));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| try_in(&dir))
}

fn command_check(name: &'static str, command: &str, what: &str) -> Check {
    let program = command.split_whitespace().next().unwrap_or("");
    // `cmd /C ...` runs through cmd.exe: the real program is not looked up here.
    match resolve_program(program) {
        Some(path) => {
            let shim = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
            if shim {
                Check::new(name, Status::Warn, format!("{what} `{program}` resolves to a .cmd/.bat shim ({}); memb starts commands without a shell, so give it as `cmd /C ...`", path.display()))
            } else {
                Check::new(name, Status::Ok, format!("{what} `{program}` found at {}", path.display()))
            }
        }
        None => Check::new(name, Status::Warn, format!("{what} `{program}` was not found on PATH; curation with it would fail")),
    }
}

fn scheduled_task_check() -> Check {
    const NAME: &str = "MemberBerriesDaemon";
    #[cfg(windows)]
    {
        let out = std::process::Command::new("schtasks")
            .args(["/Query", "/TN", NAME, "/V", "/FO", "LIST"])
            .stdin(std::process::Stdio::null())
            .output();
        let Ok(out) = out else { return Check::new("scheduled task", Status::Info, "could not query the Task Scheduler") };
        if !out.status.success() {
            return Check::new("scheduled task", Status::Info, format!("{NAME} is not installed (optional; see scripts/install-daemon-task.ps1)"));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |label: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(label))
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        let state = field("Status:");
        let target = field("Task To Run:");
        let exe = target.trim_matches('"').split("\" ").next().unwrap_or("").trim_matches('"').to_owned();
        let exe = exe.split(" daemon").next().unwrap_or("").trim_matches('"').to_owned();
        if !exe.is_empty() && !Path::new(&exe).is_file() {
            return Check::new("scheduled task", Status::Warn, format!("{NAME} ({state}) points at a program that does not exist: {exe}; reinstall it with scripts/install-daemon-task.ps1"));
        }
        Check::new("scheduled task", Status::Ok, format!("{NAME}: {state}; runs {exe}"))
    }
    #[cfg(not(windows))]
    Check::new("scheduled task", Status::Info, format!("{NAME}: not applicable on this platform"))
}

fn hook_files_check() -> Check {
    let Ok(exe) = std::env::current_exe() else {
        return Check::new("hook files", Status::Info, "cannot locate this program");
    };
    let in_plugin = exe.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "server"));
    let capture = if std::env::var("MEMB_SCROLL_CAPTURE").ok().as_deref() == Some("1") { "capture is ON" } else { "capture is off (MEMB_SCROLL_CAPTURE is not 1)" };
    let kill = if std::env::var("MEMB_NO_MEMORY").ok().as_deref() == Some("1") { "; kill switch MEMB_NO_MEMORY is set" } else { "" };
    if !in_plugin {
        return Check::new("hook files", Status::Info, format!("not running from a plugin package's server folder; {capture}{kill}"));
    }
    let root = exe.parent().and_then(Path::parent).unwrap_or(Path::new(""));
    let found: Vec<&str> = [("hooks/hooks.json", "Claude/Codex"), ("hooks.json", "Antigravity")]
        .iter()
        .filter(|(rel, _)| root.join(rel).is_file())
        .map(|(_, host)| *host)
        .collect();
    if found.is_empty() {
        Check::new("hook files", Status::Warn, format!("this package has no hooks file; {capture}{kill}"))
    } else {
        Check::new("hook files", Status::Ok, format!("hooks file present ({}); {capture}{kill}", found.join(", ")))
    }
}

/// The hint when no backend is configured: which build fits this machine, or that none does.
fn memb_embed_hint() -> String {
    let hw = memb_win::probe_hardware();
    match crate::models::recommend(&hw) {
        Ok(v) => format!(
            "no embedding backend: lexical-only search. `memb models install` downloads the EmbeddingGemma 2 {} build ({:.0} MB, verified, {}-wide vectors) that fits this machine, and memb then uses it automatically",
            v.name(),
            v.manifest().total_bytes() as f64 / 1e6,
            v.mrl_dim()
        ),
        Err(reason) => format!(
            "no embedding backend: lexical-only search, and this machine is below the floor for a local model ({reason})"
        ),
    }
}

fn embedding_check() -> Check {
    if std::env::var("MEMB_EMBED_DISABLE").ok().as_deref() == Some("1") {
        return Check::new("embeddings", Status::Info, "disabled: lexical-only search");
    }
    if let Some(dir) = std::env::var_os("MEMB_GEMMA_MODEL_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        let ready = dir.join("tokenizer.json").is_file()
            && dir.join("onnx").join("model_q4.onnx").is_file()
            && dir.join("onnx").join("model_q4.onnx_data").is_file();
        return if ready {
            Check::new("embeddings", Status::Ok, format!("in-process EmbeddingGemma 2 model files present in {}", dir.display()))
        } else {
            Check::new("embeddings", Status::Warn, format!("MEMB_GEMMA_MODEL_DIR={} lacks tokenizer.json, onnx/model_q4.onnx or onnx/model_q4.onnx_data (run `memb models install` to fetch the verified model); search falls back to lexical", dir.display()))
        };
    }
    let port = std::env::var("MEMB_EMBED_PORT").ok().and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0);
    match port {
        Some(port) => {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            // Only a connection attempt: nothing is sent to the listener.
            match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)) {
                Ok(_) => Check::new("embeddings", Status::Ok, format!("something is listening on 127.0.0.1:{port} (nothing was sent to it)")),
                Err(_) => Check::new("embeddings", Status::Warn, format!("nothing is listening on 127.0.0.1:{port}; search falls back to lexical")),
            }
        }
        None => Check::new(
            "embeddings",
            Status::Info,
            memb_embed_hint(),
        ),
    }
}

fn resource_check() -> Check {
    let hw = memb_win::probe_hardware();
    let tier = memb_win::choose_tier(&hw);
    let job = {
        #[cfg(windows)]
        {
            if memb_win::KillOnCloseJob::new().is_ok() { "job objects available (the daemon and MCP server apply a 2 GiB cap)" } else { "job objects NOT available" }
        }
        #[cfg(not(windows))]
        {
            "job objects not applicable"
        }
    };
    let private = memb_win::process_private_mib().map_or("?".to_owned(), |m| m.to_string());
    let status = if job.contains("NOT") { Status::Warn } else { Status::Ok };
    Check::new(
        "resources",
        status,
        format!(
            "{job}; {} physical cores, {} MiB RAM, AVX-512 {}, AVX2+FMA {} -> tier {tier:?} (model idle unload {} s); this process uses {private} MiB",
            hw.physical_cores,
            hw.total_ram_mib,
            hw.avx512,
            hw.avx2_fma,
            tier.idle_unload_secs()
        ),
    )
}

/// The whole health check. `store` is `None` when it could not be opened.
pub fn doctor(root: &Path, config: &Result<Config, String>, store: Option<&Store>, now_ms: u64) -> Vec<Check> {
    let mut checks = vec![Check::new("version", Status::Ok, format!("memb {}", env!("CARGO_PKG_VERSION")))];
    checks.push(if root.is_dir() {
        Check::new("data dir", Status::Ok, root.display().to_string())
    } else {
        Check::new("data dir", Status::Fail, format!("{} does not exist", root.display()))
    });
    match store {
        Some(store) => {
            let size = std::fs::metadata(root.join("memory.redb")).map(|m| m.len()).unwrap_or(0);
            let notes = store.load_all_notes().map(|n| n.len());
            checks.push(match notes {
                Ok(n) => Check::new("database", Status::Ok, format!("memory.redb opens (lock acquired), {n} notes, {} KiB", size / 1024)),
                Err(_) => Check::new("database", Status::Fail, "memory.redb opened but its notes could not be read"),
            });
        }
        None => checks.push(Check::new("database", Status::Fail, "memory.redb could not be opened (is another memb holding it, or is the data dir read-only?)")),
    }
    match config {
        Ok(cfg) => {
            checks.push(Check::new("config", Status::Ok, "config.toml is valid (or absent: defaults)"));
            checks.push(embedding_check());
            checks.push(command_check("curation model", &cfg.curator.cli_command, "main model"));
            if let Some(fallback) = cfg.curator.fallback() {
                checks.push(command_check("curation fallback", fallback, "fallback model"));
            }
            let mut curation = format!(
                "daemon curation {}, auto-apply {} (max {} per run)",
                if cfg.curator.daemon_enabled { "ON" } else { "off" },
                if cfg.curator.auto_apply { "ON" } else { "off" },
                cfg.curator.max_auto_edits
            );
            if let Some(store) = store {
                let slots = 1 + usize::from(cfg.curator.fallback().is_some());
                for slot in 0..slots {
                    if let Ok((until, strikes)) = store.with_db(|db| memb_curator::links::quota_backoff(db, slot)) {
                        if until > now_ms {
                            let mins = (until - now_ms).div_ceil(60_000);
                            curation.push_str(&format!("; model {} waits for its quota ~{mins} min (hit {strikes}x)", slot + 1));
                        }
                    }
                }
                if store.with_db(memb_curator::links::auto_apply_paused).unwrap_or(false) {
                    curation.push_str("; auto-apply is PAUSED (memb proposals resume)");
                }
            }
            checks.push(Check::new("curation", Status::Info, curation));
        }
        Err(_) => checks.push(Check::new("config", Status::Fail, "config.toml is not valid (run any memb command to see which setting)")),
    }
    checks.push(resource_check());
    checks.push(hook_files_check());
    checks.push(scheduled_task_check());
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_found_by_path_and_extension_and_a_missing_one_is_not() {
        // cmd.exe is on every Windows PATH; a made-up name is nowhere.
        #[cfg(windows)]
        assert!(resolve_program("cmd").is_some());
        assert!(resolve_program("memb-no-such-program-xyz").is_none());
        assert!(resolve_program("C:/no/such/dir/prog").is_none());
    }

    #[test]
    fn check_lines_carry_their_status() {
        assert!(Check::new("x", Status::Ok, "fine").line().starts_with("ok   x: fine"));
        assert!(Check::new("x", Status::Fail, "bad").line().starts_with("FAIL x: bad"));
    }
}
