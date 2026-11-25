//! `memb curate` and `memb proposals` through the real binary, with a fake command-line
//! model (`cmd /C type reply.json`) standing in for `claude -p`. Windows only, like memb.
#![cfg(windows)]
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use memb_core::{Draft, Kind};
use memb_scroll::{Role, ScrollLimits};
use memb_store::Store;
use serde_json::{json, Value};

const CHUNK: &str = "chk_sess1_0000";

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str, extra_config: &str) -> Self {
        let root = std::env::temp_dir().join(format!("memb-curate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config.toml"),
            format!("[scroll]\nmax_messages = 10\nretain_tail = 2\n[curator]\ncli_command = \"cmd /C type reply.json\"\n{extra_config}"),
        )
        .unwrap();
        Self(root)
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_memb"));
        c.args(args)
            .current_dir(&self.0)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| !k.starts_with("MEMB_")))
            .env("MEMB_DATA_DIR", &self.0);
        c
    }
    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
        child.wait_with_output().unwrap()
    }
    /// The fake model's single reply. An array is taken as the curation proposals and is
    /// wrapped in an object that also answers the summary request, so one file serves
    /// both calls. It lives in the sandbox directory: the command runs there, so finding
    /// it at all proves the sandbox working directory is used.
    fn reply(&self, value: &Value) {
        let body = match value {
            Value::Array(_) => json!({"summary": ["A synthetic summary bullet"], "explicit_signal": false, "proposals": value}),
            other => other.clone(),
        };
        let sandbox = self.0.join("curator-sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(sandbox.join("reply.json"), body.to_string()).unwrap();
    }
    fn store(&self) -> Store {
        Store::open(&self.0).unwrap()
    }
    /// A topic note plus one finished chunk (`chk_sess1_0000`) of 10 messages.
    fn seed(&self) -> u64 {
        let store = self.store();
        let topic = store
            .remember(Draft {
                title: "Release process".into(),
                desc: "How releases are cut".into(),
                kind: Kind::Topic,
                parents: vec![],
                properties: Default::default(),
            })
            .unwrap();
        let limits = ScrollLimits { max_messages: 10, retain_tail: 2, ..ScrollLimits::default() };
        let mut chunks = 0;
        for i in 0..10 {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            let made = store
                .with_db(|db| memb_scroll::append_message_with(db, "sess1", "cc", role, format!("synthetic message {i} about releases"), true, &limits))
                .unwrap();
            chunks += usize::from(made.is_some());
        }
        assert_eq!(chunks, 1);
        topic.id
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn json_line(bytes: &[u8]) -> Value {
    serde_json::from_str(text(bytes).trim()).unwrap()
}

#[test]
fn curation_end_to_end_makes_pending_proposals_and_applies_only_on_confirmation() {
    let dir = Dir::new("e2e", "");
    let topic = dir.seed();
    dir.reply(&json!([
        {"action":"KNOWLEDGE_UPSERT","title":"Release checklist","desc":"Run the gate before tagging","parents":[topic],"source_chunks":[CHUNK]},
        {"action":"UPDATE","id":topic,"patch":{"desc":"How releases are cut and verified"},"source_chunks":[CHUNK]},
        {"action":"KNOWLEDGE_UPSERT","title":"Unwanted","desc":"Will be rejected","parents":[topic],"source_chunks":[CHUNK]},
        {"action":"KNOWLEDGE_UPSERT","title":"Invented source","desc":"Cites a chunk that was never sent","parents":[topic],"source_chunks":["chk_other_0000"]},
        {"action":"DEFER","source_chunks":[]}
    ]));

    // A dry run shows the size, sends nothing and leaves the chunk staged.
    let out = dir.run(&["curate", "--dry-run"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let summary = json_line(&out.stdout);
    assert_eq!((summary["dry_run"].clone(), summary["chunks_sent"].clone(), summary["proposals"].clone()), (json!(true), json!(1), json!([])));
    assert!(summary["prompt_bytes"].as_u64().unwrap() > 100);
    assert!(!text(&out.stderr).contains("sending"), "a dry run must not send anything");
    assert_eq!(summary["summarized"], 1, "a dry run reports how many summaries it would ask for");
    assert!(dir.run(&["proposals"], "").stdout.is_empty());
    let unsummarized = || dir.store().with_db(|db| memb_scroll::list_unsummarized_chunks(db, 10)).unwrap().len();
    assert_eq!(unsummarized(), 1, "a dry run summarizes nothing");

    // The real run sends the chunk to the configured command and records proposals only.
    let out = dir.run(&["curate"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let summary = json_line(&out.stdout);
    assert_eq!(summary["proposals"], json!([1, 2, 3]));
    assert_eq!(summary["skipped"], 1, "the invented source is dropped, DEFER is ignored");
    assert_eq!(summary["summarized"], 1);
    // Layer 1: the chunk now carries the model's summary and directive flag.
    let chunk = dir.store().with_db(|db| memb_scroll::list_unembedded_chunks(db, 10)).unwrap();
    assert_eq!(chunk.len(), 1, "summarized, and with no embedding backend configured it has no vector");
    assert_eq!(chunk[0].summary.as_deref(), Some("- A synthetic summary bullet"));
    assert_eq!(chunk[0].explicit_signal, Some(false));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("sending 1 chunk(s)") && stderr.contains("`cmd`"), "{stderr}");
    assert_eq!(dir.store().load_all_notes().unwrap().len(), 1, "nothing was written to the notes");

    // The chunk was handled: another run has nothing to send.
    let again = json_line(&dir.run(&["curate"], "").stdout);
    assert_eq!(again["chunks_sent"], 0);

    let listed = text(&dir.run(&["proposals", "--status", "pending"], "").stdout);
    assert_eq!(listed.lines().count(), 3);
    let shown = json_line(&dir.run(&["proposals", "show", "1"], "").stdout);
    assert_eq!(shown["action"]["title"], "Release checklist");
    assert_eq!(shown["sources"][0]["chunk_id"], CHUNK);
    assert!(shown["action"].get("source_chunks").is_none());

    // A wrong confirmation changes nothing.
    let out = dir.run(&["proposals", "apply", "1"], "yes\n");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(dir.store().load_all_notes().unwrap().len(), 1);
    assert_eq!(text(&dir.run(&["proposals", "--status", "pending"], "").stdout).lines().count(), 3);

    // The typed confirmation creates the note with its sources, once.
    let out = dir.run(&["proposals", "apply", "1"], "APPLY 1\n");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let notes = dir.store().load_all_notes().unwrap();
    assert_eq!(notes.len(), 2);
    let created = notes.values().find(|n| n.title == "Release checklist").unwrap();
    assert_eq!(created.properties["source_chunks"][0]["chunk_id"], CHUNK);
    assert_eq!(created.properties["source_chunks"][0]["session_id"], "sess1");
    let again = dir.run(&["proposals", "apply", "1"], "APPLY 1\n");
    assert_eq!(again.status.code(), Some(1));
    assert!(text(&again.stderr).contains("not pending"), "{}", text(&again.stderr));
    assert_eq!(dir.store().load_all_notes().unwrap().len(), 2);

    // An update is applied in place and stamped with the same provenance.
    assert!(dir.run(&["proposals", "apply", "2"], "APPLY 2\n").status.success());
    let topic_note = dir.store().inspect(topic).unwrap().unwrap();
    assert_eq!(topic_note.desc, "How releases are cut and verified");
    assert_eq!(topic_note.properties["source_chunks"][0]["chunk_id"], CHUNK);

    // A rejected proposal can no longer be applied.
    assert!(dir.run(&["proposals", "reject", "3"], "").status.success());
    assert_eq!(dir.run(&["proposals", "apply", "3"], "APPLY 3\n").status.code(), Some(1));
    assert!(dir.store().load_all_notes().unwrap().values().all(|n| n.title != "Unwanted"));
    let statuses: Vec<String> = text(&dir.run(&["proposals"], "").stdout)
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["rejected", "applied", "applied"]);
}

#[test]
fn a_failing_or_unusable_command_changes_nothing_and_keeps_the_evidence() {
    let dir = Dir::new("fail", "");
    dir.seed();
    // Not JSON at all.
    dir.reply(&json!("plain text, not an action list"));
    let out = dir.run(&["curate"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("curate failed"), "{}", text(&out.stderr));
    assert!(!text(&out.stderr).contains("plain text"), "model output is never echoed");
    // Still staged: a later run with a good answer sees the same chunk.
    dir.reply(&json!([]));
    let second = dir.run(&["curate"], "");
    assert!(second.status.success(), "{}", text(&second.stderr));
    let summary = json_line(&second.stdout);
    assert_eq!((summary["chunks_sent"].clone(), summary["proposals"].clone()), (json!(1), json!([])));
    assert_eq!(json_line(&dir.run(&["curate"], "").stdout)["chunks_sent"], 0);

    // A command that fails, and a program that does not exist.
    let dir = Dir::new("exit", "");
    dir.seed();
    std::fs::write(dir.0.join("config.toml"), "[scroll]\nmax_messages = 10\nretain_tail = 2\n[curator]\ncli_command = \"cmd /C exit 3\"\n").unwrap();
    let out = dir.run(&["curate"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("exit 3"), "{}", text(&out.stderr));
    std::fs::write(dir.0.join("config.toml"), "[scroll]\nmax_messages = 10\nretain_tail = 2\n[curator]\ncli_command = \"memb-no-such-program-xyz -p\"\n").unwrap();
    let out = dir.run(&["curate"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("could not start"), "{}", text(&out.stderr));
    // The evidence is still there for the next attempt.
    std::fs::write(dir.0.join("config.toml"), "[scroll]\nmax_messages = 10\nretain_tail = 2\n[curator]\ncli_command = \"cmd /C type reply.json\"\n").unwrap();
    dir.reply(&json!([]));
    assert_eq!(json_line(&dir.run(&["curate"], "").stdout)["chunks_sent"], 1);
}

#[test]
fn curation_refuses_to_run_inside_a_curation_session_and_rejects_bad_arguments() {
    let dir = Dir::new("guard", "");
    dir.seed();
    dir.reply(&json!([]));
    let mut child = dir.command(&["curate"]).env("MEMB_INTERNAL", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let out = {
        let _ = child.stdin.take();
        child.wait_with_output().unwrap()
    };
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("inside a curation session"), "{}", text(&out.stderr));
    assert_eq!(json_line(&dir.run(&["curate", "--dry-run"], "").stdout)["chunks_sent"], 1, "the guarded run staged nothing away");
    for args in [&["curate", "--now"][..], &["proposals", "apply"], &["proposals", "apply", "0"], &["proposals", "--limit", "500"], &["proposals", "--status", "open"]] {
        assert_eq!(dir.run(args, "").status.code(), Some(2), "{args:?}");
    }
    assert_eq!(dir.run(&["proposals", "show", "9"], "").status.code(), Some(1));

    // The model command (MEMB_INTERNAL=1) cannot approve or reject proposals.
    let topic = dir.store().remember(Draft { title: "T".into(), desc: "d".into(), kind: Kind::Topic, parents: vec![], properties: Default::default() }).unwrap().id;
    dir.reply(&json!([{"action":"KNOWLEDGE_UPSERT","title":"Guarded","desc":"Needs a human","parents":[topic],"source_chunks":[CHUNK]}]));
    assert_eq!(json_line(&dir.run(&["curate"], "").stdout)["proposals"], json!([1]));
    for verb in ["apply", "reject"] {
        let mut child = dir.command(&["proposals", verb, "1"]).env("MEMB_INTERNAL", "1").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let _ = child.stdin.take().unwrap().write_all(b"APPLY 1\n");
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{verb}");
        assert!(text(&out.stderr).contains("inside a curation session"), "{}", text(&out.stderr));
    }
    assert_eq!(json_line(text(&dir.run(&["proposals"], "").stdout).lines().next().unwrap().as_bytes())["status"], "pending");
    assert!(dir.store().load_all_notes().unwrap().values().all(|n| n.title != "Guarded"));

    // A full pending list stops a run before anything is sent, and the chunk stays staged.
    let store = dir.store();
    let limits = ScrollLimits { max_messages: 10, retain_tail: 2, ..ScrollLimits::default() };
    for i in 0..10 {
        store.with_db(|db| memb_scroll::append_message_with(db, "sess2", "cc", Role::User, format!("another synthetic message {i}"), true, &limits)).unwrap();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    for _ in 0..memb_curator::ledger::MAX_PENDING - 1 {
        store.with_db(|db| memb_curator::ledger::add(db, now, json!({"action":"DEFER"}), vec![])).unwrap();
    }
    drop(store);
    let out = dir.run(&["curate"], "");
    assert_eq!(out.status.code(), Some(1), "{} / {}", text(&out.stdout), text(&out.stderr));
    assert!(text(&out.stderr).contains("too many pending proposals"), "{}", text(&out.stderr));
    assert!(!text(&out.stderr).contains("sending"), "nothing may be sent");
    assert_eq!(json_line(&dir.run(&["curate", "--dry-run"], "").stdout)["chunks_sent"], 1, "a dry run still reports, it sends nothing");
}

/// With an embedding backend configured, each summary also gets a vector tagged with the
/// model identity (Layer 1); the chunk text itself is never sent to the embedding service,
/// only the model-written summary.
#[test]
fn summaries_get_an_embedding_tagged_with_the_model_identity() {
    use std::io::{Read, Write as _};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::{Arc, Mutex};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // The client writes the request in several pieces: read the whole of it.
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).into_owned();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            seen.lock().unwrap().push(String::from_utf8_lossy(&request).into_owned());
            let body = r#"{"embedding":[0.5,-0.25,0.125,1.0]}"#;
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    let dir = Dir::new("embed", &format!("\n[embeddings]\nport = {port}\nmodel_id = \"synthetic-v1\"\n"));
    dir.seed();
    dir.reply(&json!([]));
    let out = dir.run(&["curate"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let summary = json_line(&out.stdout);
    assert_eq!((summary["summarized"].clone(), summary["embedded"].clone()), (json!(1), json!(1)));
    let chunk = dir.store().with_db(|db| memb_scroll::read_chunk(db, CHUNK, 0, 1)).unwrap().unwrap();
    assert_eq!(chunk["session_id"], "sess1");
    let stored = dir
        .store()
        .with_db(|db| {
            let r = db.begin_read().map_err(|e| e.to_string())?;
            let t = r.open_table(memb_scroll::CHUNKS_TABLE).map_err(|e| e.to_string())?;
            let raw = t.get(CHUNK).map_err(|e| e.to_string())?.ok_or("missing")?.value().to_vec();
            serde_json::from_slice::<memb_scroll::ScrollChunk>(&raw).map_err(|e| e.to_string())
        })
        .unwrap();
    let vector = stored.summary_vec.expect("the summary has a vector");
    assert_eq!((vector.model.as_str(), vector.data.len()), ("synthetic-v1", 4));
    // Only the summary was sent for embedding, not the conversation text.
    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("A synthetic summary bullet"), "{sent}");
    assert!(!sent.contains("synthetic message"), "chunk text must not reach the embedding service: {sent}");
    // Nothing is left to embed, so a second run adds no vector and sends no embedding request.
    let before = bodies.lock().unwrap().len();
    dir.run(&["curate"], "");
    assert_eq!(bodies.lock().unwrap().len(), before);
}
