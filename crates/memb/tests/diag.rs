//! The diagnostics commands, through the real binary (task T021). Every command is run as
//! a process; the doctor and `inspect` output is checked for private text.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use memb_core::{Draft, Kind};
use memb_scroll::{Role, ScrollLimits};
use memb_store::Store;
use serde_json::Value;

const NOTE_MARKER: &str = "SECRET-NOTE-MARKER-4471";
const CHAT_MARKER: &str = "SECRET-CHAT-MARKER-9023";

struct Env {
    root: PathBuf,
    data: PathBuf,
}

impl Env {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("memb-diag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        Self { root, data }
    }

    fn store(&self) -> Store {
        Store::open(&self.data).unwrap()
    }

    fn seed(&self) {
        let store = self.store();
        store
            .remember(Draft {
                title: format!("title {NOTE_MARKER}"),
                desc: format!("desc {NOTE_MARKER}"),
                kind: Kind::Topic,
                parents: vec![],
                properties: BTreeMap::new(),
            })
            .unwrap();
        store
            .with_db(|db| {
                for i in 0..3 {
                    memb_scroll::append_message_with(db, "cc_sess1", "cc", Role::User, format!("{CHAT_MARKER} {i}"), true, &ScrollLimits::default())?;
                }
                memb_scroll::should_inject_and_mark(db, "cc_sess1", 5, 7)?;
                // 90 messages make a chunk.
                for i in 0..90 {
                    memb_scroll::append_message_with(db, "cc_sess2", "cc", Role::User, format!("{CHAT_MARKER} chunk {i}"), true, &ScrollLimits::default())?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_memb"));
        command
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| !k.starts_with("MEMB_")))
            .env("MEMB_DATA_DIR", &self.data)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            command.env(k, v);
        }
        command.output().unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn inspect_session_shows_counts_epoch_and_injected_notes_but_no_text() {
    let e = Env::new("inspect");
    e.seed();
    for id in ["sess1", "cc_sess1"] {
        let o = e.run(&["inspect", "session", id], &[]);
        assert!(o.status.success(), "{}", err(&o));
        let v: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
        assert_eq!(v["session_key"], "cc_sess1");
        assert_eq!((v["messages"].as_u64(), v["max_messages"].as_u64()), (Some(3), Some(90)));
        assert_eq!((v["total_chunks"].as_u64(), v["dedup_epoch"].as_u64()), (Some(0), Some(0)));
        assert_eq!(v["injected_note_ids"], serde_json::json!([5]));
        assert!(!out(&o).contains(CHAT_MARKER) && !err(&o).contains(CHAT_MARKER));
    }
    // sess2 rolled over once (90 messages): one chunk, ten messages kept.
    let v: Value = serde_json::from_str(out(&e.run(&["inspect", "session", "sess2"], &[])).lines().next().unwrap()).unwrap();
    assert_eq!((v["messages"].as_u64(), v["total_chunks"].as_u64()), (Some(10), Some(1)));
    // Unknown and malformed ids fail without echoing anything stored.
    assert_eq!(e.run(&["inspect", "session", "nope"], &[]).status.code(), Some(1));
    assert_eq!(e.run(&["inspect", "session", "../x"], &[]).status.code(), Some(1));
    assert_eq!(e.run(&["inspect", "bogus", "x"], &[]).status.code(), Some(2));
    assert_eq!(e.run(&["inspect"], &[]).status.code(), Some(2));
}

#[test]
fn chunk_read_is_gated_like_the_scroll_tool() {
    let e = Env::new("chunk");
    e.seed();
    let chunk = "chk_cc_sess2_0000";
    // Capture off: refused, and nothing is printed.
    let off = e.run(&["chunk", "read", chunk], &[]);
    assert_eq!(off.status.code(), Some(1));
    assert!(out(&off).is_empty() && !err(&off).contains(CHAT_MARKER));
    // The kill switch wins even with capture on.
    let killed = e.run(&["chunk", "read", chunk], &[("MEMB_SCROLL_CAPTURE", "1"), ("MEMB_NO_MEMORY", "1")]);
    assert_eq!(killed.status.code(), Some(1));
    assert!(out(&killed).is_empty());
    // Opted in: a bounded page of the chunk.
    let on = e.run(&["chunk", "read", chunk, "--offset", "2", "--limit", "3"], &[("MEMB_SCROLL_CAPTURE", "1")]);
    assert!(on.status.success(), "{}", err(&on));
    let v: Value = serde_json::from_str(out(&on).trim()).unwrap();
    assert_eq!((v["total_messages"].as_u64(), v["offset"].as_u64()), (Some(90), Some(2)));
    assert_eq!(v["messages"].as_array().unwrap().len(), 3);
    assert!(out(&on).contains(CHAT_MARKER));
    // Unknown chunk, bad id and bad flags.
    assert_eq!(e.run(&["chunk", "read", "chk_nope"], &[("MEMB_SCROLL_CAPTURE", "1")]).status.code(), Some(1));
    assert_eq!(e.run(&["chunk", "read", "../etc"], &[("MEMB_SCROLL_CAPTURE", "1")]).status.code(), Some(2));
    assert_eq!(e.run(&["chunk", "read", chunk, "--limit", "51"], &[("MEMB_SCROLL_CAPTURE", "1")]).status.code(), Some(2));
    assert_eq!(e.run(&["chunk", "list"], &[]).status.code(), Some(2));
}

#[test]
fn history_is_the_revisions_command_under_its_phase_4_name() {
    let e = Env::new("history");
    e.seed();
    let history = e.run(&["history", "--limit", "5"], &[]);
    let revisions = e.run(&["revisions", "--limit", "5"], &[]);
    assert!(history.status.success(), "{}", err(&history));
    assert_eq!(out(&history), out(&revisions));
    let line: Value = serde_json::from_str(out(&history).lines().next().unwrap()).unwrap();
    assert!(line.get("revision").is_some() && line.get("op").is_some(), "{line}");
    assert!(!out(&history).contains(NOTE_MARKER), "revision lines carry metadata, not note text");
    assert_eq!(e.run(&["history", "--limit", "0"], &[]).status.code(), Some(2));
}

#[test]
fn routine_run_now_runs_one_pass_regardless_of_the_schedule() {
    let e = Env::new("routine");
    e.seed();
    // The first pass and an immediate second one (the schedule's cooldown would refuse it).
    for _ in 0..2 {
        let o = e.run(&["routine", "run-now"], &[]);
        assert!(o.status.success(), "{}", err(&o));
        let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
        assert_eq!(v["ran"], true);
        assert!(v["swept"].is_u64() && v["staged"].is_u64());
    }
    // Staging happened: the chunk rolled over earlier is now staged evidence.
    let dry = e.run(&["curate", "--dry-run"], &[]);
    assert!(dry.status.success(), "{}", err(&dry));
    assert_eq!(e.run(&["routine"], &[]).status.code(), Some(2));
    assert_eq!(e.run(&["routine", "now"], &[]).status.code(), Some(2));
}

#[test]
fn doctor_reports_health_without_any_note_or_chat_text() {
    let e = Env::new("doctor");
    e.seed();
    let o = e.run(&["doctor"], &[]);
    let text = out(&o);
    assert!(o.status.success(), "{text}\n{}", err(&o));
    for needle in ["ok   version: memb ", "ok   data dir:", "ok   database:", "ok   config:", "resources:", "hook files:", "scheduled task:", "curation model:", "check(s), 0 failed"] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    assert!(text.contains("1 notes"), "{text}");
    for private in [NOTE_MARKER, CHAT_MARKER] {
        assert!(!text.contains(private) && !err(&o).contains(private), "doctor printed private text");
    }
    // A bad config is a failure that names the setting's file, never the value.
    std::fs::write(e.data.join("config.toml"), "[storage]\nmax_notes = \"HUNTER2-VALUE\"\n").unwrap();
    let bad = e.run(&["doctor"], &[]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(out(&bad).contains("FAIL config:"), "{}", out(&bad));
    assert!(!out(&bad).contains("HUNTER2-VALUE") && !err(&bad).contains("HUNTER2-VALUE"));
    // A model command that does not exist is a warning, not a failure.
    std::fs::write(e.data.join("config.toml"), "[curator]\ncli_command = \"memb-no-such-model-xyz -p\"\nfallback_command = \"\"\n").unwrap();
    let warn = e.run(&["doctor"], &[]);
    assert!(warn.status.success(), "{}", out(&warn));
    assert!(out(&warn).contains("warn curation model:") && out(&warn).contains("was not found on PATH"), "{}", out(&warn));
    // Extra arguments are a usage error.
    assert_eq!(e.run(&["doctor", "--json"], &[]).status.code(), Some(2));
}
