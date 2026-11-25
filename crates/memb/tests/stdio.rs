use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Client {
    fn start(path: &Path) -> Self {
        Self::start_with_capture(path, false)
    }
    fn start_with_capture(path: &Path, capture: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_memb"));
        if capture {
            command.env("MEMB_SCROLL_CAPTURE", "1");
        } else {
            command.env_remove("MEMB_SCROLL_CAPTURE");
        }
        let mut child = command
            .arg("mcp")
            .env("MEMB_DATA_DIR", path)
            .env_remove("MEMB_NO_MEMORY")
            .env_remove("MEMB_INTERNAL")
            .env_remove("MEMB_IGNORE_DIRS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut c = Self {
            child,
            input,
            output,
        };
        let init = c.rpc("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}));
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        writeln!(
            c.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        c
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":10,"method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let result: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(result["id"], 10);
        result
    }
    fn tool(&mut self, name: &str, args: Value) -> (bool, Value) {
        let result = self.rpc("tools/call", json!({"name":name,"arguments":args}));
        let r = &result["result"];
        let text = r["content"][0]["text"].as_str().unwrap();
        let error = r["isError"].as_bool().unwrap();
        (
            error,
            if error {
                json!(text)
            } else {
                serde_json::from_str(text).unwrap()
            },
        )
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn cli_search_falls_back_when_embedding_server_is_absent() {
    let root = std::env::temp_dir().join(format!(
        "memb-cli-search-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut client = Client::start(&root);
    let (error, _) = client.tool(
        "remember",
        json!({"title":"Synthetic apple", "desc":"red fruit", "kind":"topic", "confirmed":true}),
    );
    assert!(!error);
    drop(client);
    let output = Command::new(env!("CARGO_BIN_EXE_memb"))
        .args(["search", "apple"])
        .env("MEMB_DATA_DIR", &root)
        .env("MEMB_EMBED_PORT", "0")
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["notes"][0]["title"], "Synthetic apple");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn supervisor_configuration_does_not_contact_unrelated_listener() {
    let root = std::env::temp_dir().join(format!(
        "memb-supervisor-negative-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut client = Client::start(&root);
    let (error, _) = client.tool("remember", json!({"title":"Synthetic apple", "desc":"private synthetic text", "kind":"topic", "confirmed":true}));
    assert!(!error);
    drop(client);
    // An unrelated service is already listening. Executable/model paths alone
    // must not start supervision or send it the query or document text.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_memb"))
        .args(["search", "apple"])
        .env("MEMB_DATA_DIR", &root)
        .env_remove("MEMB_EMBED_PORT")
        .env("MEMB_EMBED_SERVER", env!("CARGO_BIN_EXE_memb"))
        .env("MEMB_EMBED_MODEL", env!("CARGO_BIN_EXE_memb"))
        .env("MEMB_EMBED_MODEL_ID", "synthetic-v1")
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["notes"][0]["title"], "Synthetic apple");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unified_graph_requires_consent_for_mutations() {
    let root = std::env::temp_dir().join(format!(
        "memb-integration-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut a = Client::start(&root);
    let mut b = Client::start(&root);
    let tools = a.rpc("tools/list", json!({}));
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "remember",
            "search",
            "inspect",
            "update",
            "supersede",
            "curator_evidence",
            "curator_preview",
            "curator_apply",
            "scroll_chunk_read",
            "retract"
        ]
    );
    assert_eq!(
        a.rpc("resources/list", json!({}))["result"]["resources"],
        json!([])
    );
    let draft = json!({"title":"Orchard","desc":"Synthetic example","kind":"topic","parents":[],"properties":{"source":"test"}});
    assert!(a.tool("remember", draft.clone()).0);
    assert!(a.tool("remember", json!({"title":"secret","desc":"sk-1234567890abcdefghijkl","kind":"topic","confirmed":true})).0);
    let mut approved = draft.clone();
    approved["confirmed"] = json!(true);
    let id = a.tool("remember", approved).1["note"]["id"]
        .as_u64()
        .unwrap();
    assert_eq!(
        b.tool("inspect", json!({"id":id})).1["note"]["properties"]["source"],
        "test"
    );
    assert!(a.tool("inspect", json!({"id":id,"project":"alpha"})).0);
    assert!(a.tool("update", json!({"id":id,"title":"Changed"})).0);
    assert_eq!(
        a.tool(
            "update",
            json!({"id":id,"title":"Changed","confirmed":true})
        )
        .1["note"]["title"],
        "Changed"
    );
    assert!(!b.tool("search", json!({"query":"Changed"})).1["notes"]
        .as_array()
        .unwrap()
        .is_empty());
    let knowledge = a.tool("remember", json!({"title":"Care","desc":"Synthetic","kind":"knowledge","parents":[id],"confirmed":true})).1["note"]["id"].as_u64().unwrap();
    let denied = a.tool("retract", json!({"id":id,"confirmed":true}));
    assert!(denied.0);
    let message = denied.1.as_str().unwrap();
    assert!(message.contains("migrate_to_topic_id"));
    assert!(message.contains("force_cascade"));
    assert!(!message.contains("Orchard"));
    let claim = a.tool("remember", json!({"title":"Water","desc":"Synthetic","kind":"claim","parents":[knowledge],"confirmed":true})).1["note"]["id"].as_u64().unwrap();
    assert!(a.tool("supersede", json!({"id":claim,"title":"Replacement","desc":"Synthetic","kind":"claim","parents":[knowledge]})).0);
    let replacement = a.tool("supersede", json!({"id":claim,"title":"Replacement","desc":"Synthetic","kind":"claim","parents":[knowledge],"confirmed":true}));
    assert!(!replacement.0);
    let replacement_id = replacement.1["note"]["id"].as_u64().unwrap();
    assert!(a.tool("retract", json!({"id":replacement_id})).0);
    assert!(
        !a.tool("retract", json!({"id":replacement_id,"confirmed":true}))
            .0
    );
    assert_eq!(
        a.tool("inspect", json!({"id":replacement_id})).1["note"]["status"],
        "retracted"
    );
    drop(a);
    drop(b);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hooks_require_opt_in_honor_exclusions_and_deduplicate() {
    let root = std::env::temp_dir().join(format!(
        "memb-hook-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = std::env::current_dir().unwrap();
    let prompt = format!("orchard {}", "synthetic ".repeat(12));
    let invoke = |enabled: bool, disabled: bool, excluded: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_memb"));
        cmd.args(["hook", "prompt-submit", "--cli", "cc"])
            .env("MEMB_DATA_DIR", &root)
            .env_remove("MEMB_SCROLL_CAPTURE")
            .env_remove("MEMB_NO_MEMORY")
            .env_remove("MEMB_INTERNAL")
            .env_remove("MEMB_IGNORE_DIRS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if enabled {
            cmd.env("MEMB_SCROLL_CAPTURE", "1");
        }
        if disabled {
            cmd.env("MEMB_NO_MEMORY", "1");
        }
        if excluded {
            cmd.env("MEMB_IGNORE_DIRS", &cwd);
        }
        let mut child = cmd.spawn().unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{}",
            json!({"cwd":cwd,"session_id":"session1","prompt":prompt})
        )
        .unwrap();
        child.wait_with_output().unwrap()
    };
    assert!(invoke(false, false, false).stdout.is_empty());
    assert!(!root.exists());
    assert!(invoke(true, true, false).stdout.is_empty());
    assert!(invoke(true, false, true).stdout.is_empty());
    assert!(!root.exists());
    let mut client = Client::start(&root);
    assert!(
        !client
            .tool(
                "remember",
                json!({"title":"Orchard","desc":"Synthetic note","kind":"topic","confirmed":true})
            )
            .0
    );
    let first = invoke(true, false, false);
    assert!(first.status.success());
    assert!(String::from_utf8(first.stdout)
        .unwrap()
        .contains("Synthetic note"));
    let second = invoke(true, false, false);
    assert!(second.stdout.is_empty());
    assert!(
        client
            .tool(
                "scroll_chunk_read",
                json!({"chunk_id":"chk_cc_session1_0000"})
            )
            .0
    );
    drop(client);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn curator_proposals_require_preview_and_single_use_consent() {
    let root = std::env::temp_dir().join(format!(
        "memb-curator-runtime-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = memb_store::Store::open(&root).unwrap();
    store
        .with_db(|db| {
            for _ in 0..90 {
                memb_scroll::append_message(
                    db,
                    "synthetic",
                    "cc",
                    memb_scroll::Role::User,
                    "Synthetic evidence".into(),
                    true,
                )?;
            }
            Ok(())
        })
        .unwrap();
    drop(store);
    let mut c = Client::start_with_capture(&root, true);
    let evidence = c.tool("curator_evidence", json!({"chunks":[{"chunk_id":"a","session_id":"s1","summary":"synthetic","vec":{"scale":1.0,"data":[1,0]}},{"chunk_id":"b","session_id":"s2","summary":"synthetic","vec":{"scale":1.0,"data":[1,0]}}]}));
    assert!(!evidence.0);
    assert_eq!(evidence.1["packets"][0]["distinct_sessions"], 2);
    let topic = c
        .tool(
            "remember",
            json!({"title":"Topic","desc":"Synthetic","kind":"topic","confirmed":true}),
        )
        .1["note"]["id"]
        .as_u64()
        .unwrap();
    let action = json!({"action":"KNOWLEDGE_UPSERT","title":"Proposed","desc":"Synthetic","parents":[topic]});
    assert!(
        c.tool("curator_apply", json!({"proposal_id":1,"confirmed":true}))
            .0
    );
    assert!(
        c.tool(
            "curator_preview",
            json!({"action":action,"source_chunks":[["wrong","chk_synthetic_0000"]]})
        )
        .0
    );
    let preview = c.tool(
        "curator_preview",
        json!({"action":action,"source_chunks":[["synthetic","chk_synthetic_0000"]]}),
    );
    assert!(!preview.0);
    assert_eq!(preview.1["action"], action);
    let id = preview.1["proposal_id"].as_u64().unwrap();
    assert!(c.tool("curator_apply", json!({"proposal_id":id})).0);
    assert_eq!(
        c.tool("search", json!({"query":"Proposed"})).1["notes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let applied = c.tool("curator_apply", json!({"proposal_id":id,"confirmed":true}));
    assert!(!applied.0);
    let note = c.tool("inspect", json!({"id":applied.1["note_id"]}));
    assert_eq!(
        note.1["note"]["properties"]["source_chunks"][0]["chunk_id"],
        "chk_synthetic_0000"
    );
    assert!(
        c.tool("curator_apply", json!({"proposal_id":id,"confirmed":true}))
            .0
    );
    // Update and retract proposals are previewable now; a model may not ask for a cascade
    // or set provenance itself.
    assert!(!c.tool("curator_preview", json!({"action":{"action":"RETRACT","id":topic},"source_chunks":[["synthetic","chk_synthetic_0000"]]})).0);
    assert!(!c.tool("curator_preview", json!({"action":{"action":"UPDATE","id":topic,"patch":{"desc":"A revised description"}},"source_chunks":[["synthetic","chk_synthetic_0000"]]})).0);
    assert!(c.tool("curator_preview", json!({"action":{"action":"RETRACT","id":topic,"force_cascade":true},"source_chunks":[["synthetic","chk_synthetic_0000"]]})).0);
    assert!(c.tool("curator_preview", json!({"action":{"action":"UPDATE","id":topic,"patch":{"properties":{"source_chunks":[]}}},"source_chunks":[["synthetic","chk_synthetic_0000"]]})).0);
    drop(c);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mirror_is_create_only() {
    let base = std::env::temp_dir().join(format!(
        "memb-ops-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    let root = base.join("store");
    let out = base.join("out");
    std::fs::create_dir(&out).unwrap();
    let mut c = Client::start(&root);
    assert!(
        !c.tool(
            "remember",
            json!({"title":"Synthetic","desc":"Example","kind":"topic","confirmed":true})
        )
        .0
    );
    drop(c);
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_memb"))
            .arg("mirror")
            .arg("--out")
            .arg(&out)
            .env("MEMB_DATA_DIR", &root)
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    let file = out.join("topic-1.md");
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("Synthetic"));
    std::fs::write(&file, "User-owned content").unwrap();
    assert!(!run().status.success());
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "User-owned content"
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn rollback_previews_requires_confirmation_and_rejects_conflicts() {
    let root = std::env::temp_dir().join(format!(
        "memb-rollback-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut client = Client::start(&root);
    assert!(
        !client
            .tool(
                "remember",
                json!({"title":"Original","desc":"Synthetic","kind":"topic","confirmed":true})
            )
            .0
    );
    assert!(
        !client
            .tool("update", json!({"id":1,"title":"Changed","confirmed":true}))
            .0
    );
    let start = |rev: &str| {
        Command::new(env!("CARGO_BIN_EXE_memb"))
            .args(["rollback", "--rev", rev])
            .env("MEMB_DATA_DIR", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut cancel = start("2");
    writeln!(cancel.stdin.take().unwrap(), "no").unwrap();
    let cancelled = cancel.wait_with_output().unwrap();
    assert!(!cancelled.status.success());
    let text = String::from_utf8(cancelled.stdout).unwrap();
    assert!(text.contains("Original") && text.contains("Changed"));
    assert_eq!(
        client.tool("inspect", json!({"id":1})).1["note"]["title"],
        "Changed"
    );
    let mut stale = start("2");
    let mut stdout = BufReader::new(stale.stdout.take().unwrap());
    let mut preview = String::new();
    stdout.read_line(&mut preview).unwrap();
    assert!(preview.contains("Original"));
    assert!(
        !client
            .tool("update", json!({"id":1,"title":"Later","confirmed":true}))
            .0
    );
    writeln!(stale.stdin.take().unwrap(), "ROLLBACK 2").unwrap();
    assert!(!stale.wait().unwrap().success());
    assert_eq!(
        client.tool("inspect", json!({"id":1})).1["note"]["title"],
        "Later"
    );
    let mut success = start("3");
    writeln!(success.stdin.take().unwrap(), "ROLLBACK 3").unwrap();
    let result = success.wait_with_output().unwrap();
    assert!(result.status.success());
    assert_eq!(
        client.tool("inspect", json!({"id":1})).1["note"]["title"],
        "Changed"
    );
    let unsupported = start("1").wait_with_output().unwrap();
    assert!(!unsupported.status.success());
    drop(client);
    std::fs::remove_dir_all(root).unwrap();
}
