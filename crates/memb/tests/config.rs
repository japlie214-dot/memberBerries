//! `config.toml` through the real binary: limits are honored, mistakes fail closed,
//! errors never echo file contents, and environment variables win over the file.
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use memb_scroll::Role;
use memb_store::Store;
use serde_json::{json, Value};

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str, config: Option<&str>) -> Self {
        let root = std::env::temp_dir().join(format!("memb-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        if let Some(text) = config {
            std::fs::write(root.join("config.toml"), text).unwrap();
        }
        Self(root)
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_memb"));
        c.args(args)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| !k.starts_with("MEMB_")))
            .env("MEMB_DATA_DIR", &self.0);
        c
    }
    /// Run `memb mcp` with the given tool calls; returns (stdout lines as JSON, stderr, success).
    fn mcp(&self, calls: &[(&str, Value)]) -> (Vec<Value>, String, bool) {
        let mut frames = vec![
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        ];
        for (i, (name, args)) in calls.iter().enumerate() {
            frames.push(json!({"jsonrpc":"2.0","id":10+i,"method":"tools/call","params":{"name":name,"arguments":args}}));
        }
        let mut child = self.command(&["mcp"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for f in &frames {
            let _ = writeln!(stdin, "{f}");
        }
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        let lines = String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (lines, String::from_utf8(out.stderr).unwrap(), out.status.success())
    }
    fn hook(&self, event: &str, payload: Value, capture: bool) {
        let mut c = self.command(&["hook", event, "--cli", "cc"]);
        if capture {
            c.env("MEMB_SCROLL_CAPTURE", "1");
        }
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }
    fn scroll(&self, session: &str) -> Vec<(Role, String)> {
        Store::open(&self.0)
            .unwrap()
            .with_db(|db| memb_scroll::read_scroll(db, session))
            .unwrap()
            .map(|s| s.messages.into_iter().map(|m| (m.role, m.content)).collect())
            .unwrap_or_default()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn remember(title: &str) -> (&'static str, Value) {
    ("remember", json!({"title": title, "desc": "synthetic text", "kind": "topic", "confirmed": true}))
}

fn is_error(response: &Value) -> bool {
    response["result"]["isError"] == true
}

#[test]
fn storage_limit_from_config_is_enforced_through_mcp() {
    let d = Dir::new("limit", Some("[storage]\nmax_notes = 1\n"));
    let (responses, _, ok) = d.mcp(&[remember("first"), remember("second")]);
    assert!(ok);
    assert!(!is_error(&responses[1]));
    assert!(is_error(&responses[2]), "the second note must exceed max_notes = 1");
}

#[test]
fn invalid_config_stops_startup_with_a_message_that_never_echoes_values() {
    for (text, setting) in [
        ("[storage]\nmax_notes = \"SECRET-VALUE-123\"\n", "line 2"),
        ("[storage]\nmax_notes = 98765\n", "storage.max_notes"),
        ("[embeddings]\nport = 70000\n", "line 2"),
        ("[storage]\nmax_noets = 5\n", "unknown field"),
        ("[scroll]\nmax_messages = 5\n", "scroll.max_messages"),
        ("[embeddings]\nbinary = \"C:\\\\secret-path\\\\llama-server.exe\"\n", "does not start an embedding server"),
        ("[curator]\nstaging_ttl_days = 7777\n", "curator.staging_ttl_days"),
        ("[curator]\nchunk_to_chunk_threshold = 0.5\n", "thresholds are fixed"),
        ("[curator]\ncli_command = \"  \"\n", "curator.cli_command"),
        ("[scheduler]\nidle_sweep_min_messages = 5\n", "must exceed scroll.retain_tail"),
    ] {
        let d = Dir::new("bad", Some(text));
        let (responses, stderr, ok) = d.mcp(&[remember("never stored")]);
        assert!(!ok, "{text}");
        assert!(responses.is_empty(), "no protocol output on a startup failure: {text}");
        assert!(stderr.contains(setting), "{text} -> {stderr}");
        assert!(!stderr.contains("SECRET-VALUE-123") && !stderr.contains("secret-path") && !stderr.contains("98765") && !stderr.contains("70000") && !stderr.contains("7777"), "{stderr}");
        assert!(!d.0.join("memory.redb").exists(), "a bad config must not create a database");
    }
}

#[test]
fn hook_honors_scroll_limits_and_ignore_patterns_from_config() {
    let d = Dir::new(
        "scroll",
        Some("[scroll]\nmax_messages = 10\nretain_tail = 2\nignore_dirs = [\"*/private-work/*\"]\n[scheduler]\nidle_sweep_min_messages = 3\n"),
    );
    let cwd = d.0.to_str().unwrap();
    for i in 0..12 {
        d.hook("prompt-submit", json!({"session_id": "s1", "cwd": cwd, "prompt": format!("message {i}")}), true);
    }
    // 10 messages roll over into a chunk, the last 2 stay, then messages 10 and 11 follow.
    let live = d.scroll("cc_s1");
    assert_eq!(live.len(), 4, "{live:?}");
    assert_eq!(live.first().unwrap().1, "message 8");
    assert_eq!(live.last().unwrap().1, "message 11");
    let chunk = Store::open(&d.0).unwrap().with_db(|db| memb_scroll::read_chunk(db, "chk_cc_s1_0000", 0, 50)).unwrap().unwrap();
    assert_eq!(chunk["total_messages"], 10);

    // A working directory matching an ignore pattern is never captured.
    let private = d.0.join("private-work").join("repo");
    std::fs::create_dir_all(&private).unwrap();
    d.hook("prompt-submit", json!({"session_id": "s2", "cwd": private.to_str().unwrap(), "prompt": "hidden"}), true);
    assert!(d.scroll("cc_s2").is_empty());
}

#[test]
fn embedding_settings_come_from_config_and_the_environment_wins() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = |l: &TcpListener| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        l.accept().is_ok()
    };
    let search = |d: &Dir, env: &[(&str, &str)]| {
        let mut c = d.command(&["search", "apple"]);
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.stdout(Stdio::piped()).stderr(Stdio::null()).output().unwrap();
        assert!(out.status.success());
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["notes"][0]["title"], "Synthetic apple"); // lexical fallback always answers
    };
    let seed = |d: &Dir| assert!(!is_error(&d.mcp(&[remember("Synthetic apple")]).0[1]));

    // 1. The config's port is used: the loopback listener receives a connection.
    let d = Dir::new("emb-on", Some(&format!("[embeddings]\nport = {port}\nmodel_id = \"synthetic-v1\"\n")));
    seed(&d);
    search(&d, &[]);
    assert!(accepted(&listener), "config embeddings.port must be honored");
    while listener.accept().is_ok() {}

    // 2. enabled = false forces lexical search: no connection.
    let d = Dir::new("emb-off", Some(&format!("[embeddings]\nenabled = false\nport = {port}\nmodel_id = \"synthetic-v1\"\n")));
    seed(&d);
    search(&d, &[]);
    assert!(!accepted(&listener), "embeddings.enabled = false must not contact the service");

    // 3. An explicit environment variable wins over the config file.
    let d = Dir::new("emb-env", Some(&format!("[embeddings]\nport = {port}\nmodel_id = \"synthetic-v1\"\n")));
    seed(&d);
    search(&d, &[("MEMB_EMBED_PORT", "0")]);
    assert!(!accepted(&listener), "MEMB_EMBED_PORT must override embeddings.port");
}

#[test]
fn revisions_listing_shows_metadata_only_and_feeds_a_confirmed_rollback() {
    let d = Dir::new("revisions", None);
    let (r, _, ok) = d.mcp(&[
        ("remember", json!({"title": "Orchard", "desc": "original wording", "kind": "topic", "confirmed": true})),
        ("update", json!({"id": 1, "desc": "edited wording", "confirmed": true})),
    ]);
    assert!(ok && !is_error(&r[1]) && !is_error(&r[2]));

    let list = |args: &[&str]| {
        let out = d.command(args).output().unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    };
    let (code, text) = list(&["revisions"]);
    assert_eq!(code, Some(0));
    let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.iter().map(|r| r["revision"].as_u64().unwrap()).collect::<Vec<_>>(), vec![2, 1], "newest first");
    assert_eq!(rows[0]["op"], "update");
    assert_eq!(rows[0]["changed"], json!(["desc"]));
    assert!(rows[0]["rollback"].as_str().unwrap().starts_with("single-note update"));
    assert_eq!(rows[1]["rollback"], "not supported");
    assert!(!text.contains("wording"), "note text must not be printed");
    assert_eq!(list(&["revisions", "--note", "1", "--limit", "1"]).1.lines().count(), 1);
    assert_eq!(list(&["revisions", "--note", "99"]).1, "");
    for bad in [&["revisions", "--note", "0"][..], &["revisions", "--limit", "x"], &["revisions", "--limit", "0"], &["revisions", "--limit", "201"], &["revisions", "--note", "1", "--limit", "0"], &["revisions", "--bogus"]] {
        assert_eq!(list(bad).0, Some(2), "{bad:?}");
    }

    // The listed revision id drives the confirmed rollback; a wrong confirmation changes nothing.
    let rollback = |input: &str| {
        let mut child = d.command(&["rollback", "--rev", "2"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.success(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    };
    assert!(!rollback("yes\n").0);
    assert_eq!(d.mcp(&[("inspect", json!({"id": 1}))]).0[1]["result"]["content"][0]["text"].as_str().unwrap().contains("edited wording"), true);
    let (ok, shown, _) = rollback("ROLLBACK 2\n");
    assert!(ok && shown.contains("Rollback applied"), "{shown}");
    let restored = d.mcp(&[("inspect", json!({"id": 1}))]).0[1]["result"]["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(restored.contains("original wording") && !restored.contains("edited wording"), "{restored}");
    // Rolling back again is refused: the note no longer matches the revision's after-image,
    // and the reason is printed (not a generic failure), without any note text.
    let (ok, _, reason) = rollback("ROLLBACK 2\n");
    assert!(!ok);
    assert!(reason.contains("memb: rollback failed: ") && reason.contains("stale"), "{reason}");
    assert!(!reason.contains("wording"), "{reason}");
    // A creation revision and a missing revision are refused with their own reasons.
    let refuse = |rev: &str| {
        let out = d.command(&["rollback", "--rev", rev]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
        assert!(!out.status.success());
        String::from_utf8(out.stderr).unwrap()
    };
    assert!(refuse("1").contains("only single-note updates"), "creation revision");
    assert!(refuse("99").contains("revision not found"), "missing revision");
}
