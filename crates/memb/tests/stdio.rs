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
    fn start(path: &Path, scope: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_memb"))
            .arg("mcp")
            .env("MEMB_DATA_DIR", path)
            .env("MEMB_PROJECT", scope)
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
        assert_eq!(c.rpc("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}))["result"]["protocolVersion"], "2025-06-18");
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
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], 10);
        value
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
fn shared_memory_scope_consent_and_graph() {
    let root = std::env::temp_dir().join(format!(
        "memb-integration-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let save = |kind: &str, title: &str, parents: Value, confirmed| json!({"title":title,"body":"Synthetic example","kind":kind,"parents":parents,"confirmed":confirmed});
    let mut a = Client::start(&root, "alpha");
    let mut b = Client::start(&root, "alpha");
    let tools = a.rpc("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    let opener = tools
        .iter()
        .find(|t| t["name"] == "open_inspector")
        .unwrap();
    let uri = &opener["_meta"]["ui"]["resourceUri"];
    assert_eq!(opener["_meta"]["ui"]["visibility"], json!(["model"]));
    let snapshot_tool = tools
        .iter()
        .find(|t| t["name"] == "inspector_snapshot")
        .unwrap();
    assert_eq!(snapshot_tool["_meta"]["ui"]["visibility"], json!(["app"]));
    assert_eq!(&snapshot_tool["_meta"]["ui"]["resourceUri"], uri);
    let page = a.rpc("resources/read", json!({"uri":uri}));
    assert_eq!(
        page["result"]["contents"][0]["mimeType"],
        "text/html;profile=mcp-app"
    );
    let open = a.rpc(
        "tools/call",
        json!({"name":"open_inspector","arguments":{}}),
    );
    assert_eq!(
        open["result"]["structuredContent"],
        json!({"project":"alpha"})
    );
    let fresh = a.rpc(
        "tools/call",
        json!({"name":"inspector_snapshot","arguments":{}}),
    );
    assert_eq!(fresh["result"]["structuredContent"]["counts"]["total"], 0);
    assert_eq!(fresh["result"]["content"][0]["type"], "text");
    assert!(!root.exists());
    assert!(
        a.tool("remember", save("topic", "Orchard", json!([]), false))
            .0
    );
    assert!(a.tool("remember", json!({"title":"Example","body":"api_key=synthetic-placeholder-123","kind":"topic","parents":[],"confirmed":true})).0);
    assert!(!root.exists());
    assert_eq!(
        a.tool("remember", save("topic", "Orchard", json!([]), true))
            .1["note"]["id"],
        1
    );
    assert!(
        b.tool("remember", save("claim", "Bad parent", json!([1]), true))
            .0
    );
    assert_eq!(
        b.tool(
            "remember",
            save("knowledge", "Cultivation", json!([1]), true)
        )
        .1["note"]["id"],
        2
    );
    assert_eq!(
        a.tool("remember", save("claim", "Water weekly", json!([2]), true))
            .1["note"]["id"],
        3
    );
    let projects = a.rpc(
        "tools/call",
        json!({"name":"inspector_projects","arguments":{}}),
    );
    assert_eq!(
        projects["result"]["structuredContent"],
        json!({"projects":["alpha"],"active":"alpha"})
    );
    let graph = a.rpc(
        "tools/call",
        json!({"name":"inspector_graph","arguments":{"project":"alpha"}}),
    );
    assert_eq!(graph["result"]["structuredContent"]["counts"]["total"], 3);
    assert_eq!(
        graph["result"]["structuredContent"]["notes"][2]["parents"],
        json!(["2"])
    );
    let denied = a.rpc(
        "tools/call",
        json!({"name":"inspector_graph","arguments":{"project":"beta"}}),
    );
    assert_eq!(denied["result"]["isError"], true);
    let matches = b.tool("search", json!({"query":"cultivation"})).1;
    assert_eq!(matches["notes"].as_array().unwrap().len(), 2);
    assert_eq!(matches["notes"][1]["id"], 3);
    assert_eq!(
        b.tool("search", json!({"query":"orchard"})).1["notes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(a.tool("inspect", json!({"project":"beta","id":1})).0);
    assert!(Client::start(&root, "beta")
        .tool("inspect", json!({"id":1}))
        .1["note"]
        .is_null());
    drop(a);
    drop(b);
    let mut a = Client::start(&root, "alpha");
    assert_eq!(
        a.tool("inspect", json!({"id":3})).1["note"]["title"],
        "Water weekly"
    );
    assert!(a.tool("forget", json!({"id":2,"confirmed":true})).0);
    assert_eq!(
        a.tool("forget", json!({"id":3,"confirmed":true})).1["removed"],
        true
    );
    assert!(a.tool("inspect", json!({"id":3})).1["note"].is_null());
    drop(a);
    std::fs::remove_dir_all(root).unwrap();
}
