use memb_core::Scope;
use memb_store::Store;
use serde_json::{json, Value};

pub const HTML: &str = include_str!("viewer.html");
pub const URI: &str = "ui://memb/inspector-v0.3.0.html";
const MIME: &str = "text/html;profile=mcp-app";

pub fn resource() -> Value {
    json!({"uri":URI,"name":"Memb Inspector","mimeType":MIME,"description":"Read-only project memory browser"})
}

pub fn read_resource() -> Value {
    json!({"contents":[{"uri":URI,"mimeType":MIME,"text":HTML,"_meta":{"ui":{"csp":{}}}}]})
}

pub fn tools() -> [Value; 4] {
    let schema = json!({"type":"object","additionalProperties":false,"properties":{
        "project":{"type":"string","pattern":"^[a-z0-9][a-z0-9_-]{0,63}$"},
        "query":{"type":"string","maxLength":512},
        "after":{"type":"string","pattern":"^[0-9]{1,20}$"}
    }});
    [
        json!({"name":"open_inspector","description":"Open the human read-only memory inspector without sending any notes to the model. Use the requested project if supplied.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"project":schema["properties"]["project"]}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false},"_meta":{"ui":{"resourceUri":URI,"visibility":["model"]},"ui/resourceUri":URI}}),
        json!({"name":"inspector_snapshot","description":"Get one bounded page for the inspector UI; read-only.","inputSchema":schema,"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false},"_meta":{"ui":{"resourceUri":URI,"visibility":["app"]}}}),
        json!({"name":"inspector_projects","description":"List project slugs for the read-only inspector.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false},"_meta":{"ui":{"resourceUri":URI,"visibility":["app"]}}}),
        json!({"name":"inspector_graph","description":"Get a bounded read-only project graph for the inspector.","inputSchema":{"type":"object","properties":{"project":schema["properties"]["project"]},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false},"_meta":{"ui":{"resourceUri":URI,"visibility":["app"]}}}),
    ]
}

fn project(pinned: Option<&Scope>, arg: Option<&Value>) -> Result<Option<Scope>, &'static str> {
    let requested = arg
        .map(|p| {
            p.as_str()
                .ok_or("project must be text")
                .and_then(Scope::parse)
        })
        .transpose()?;
    if matches!((pinned, &requested), (Some(a), Some(b)) if a != b) {
        return Err("project differs from the configured scope");
    }
    Ok(pinned.cloned().or(requested))
}

pub fn open(pinned: Option<&Scope>, args: &Value) -> Result<Value, &'static str> {
    let empty = serde_json::Map::new();
    let args = if args.is_null() {
        &empty
    } else {
        args.as_object().ok_or("arguments must be an object")?
    };
    if args.keys().any(|k| k != "project") {
        return Err("unexpected argument");
    }
    let scope = project(pinned, args.get("project"))?;
    Ok(
        json!({"content":[{"type":"text","text":"Memb Inspector opened on compatible hosts. No notes were read."}],"structuredContent":{"project":scope.as_ref().map(Scope::as_str)}}),
    )
}

pub fn projects(
    store: &Store,
    pinned: Option<&Scope>,
    args: &Value,
) -> Result<Value, &'static str> {
    if !args.is_object() || args.as_object().is_some_and(|o| !o.is_empty()) {
        return Err("unexpected argument");
    }
    let names = match pinned {
        Some(scope) => vec![scope.as_str().to_owned()],
        None => store.projects().map_err(|_| "Cannot read project list.")?,
    };
    Ok(json!({"projects":names, "active":pinned.map(Scope::as_str)}))
}

pub fn graph(store: &Store, pinned: Option<&Scope>, args: &Value) -> Result<Value, &'static str> {
    let args = args.as_object().ok_or("arguments must be an object")?;
    if args.keys().any(|k| k != "project") {
        return Err("unexpected argument");
    }
    let scope = project(pinned, args.get("project"))?.ok_or("enter a project slug")?;
    store
        .graph(&scope)
        .map_err(|_| "Cannot read the local store. Check permissions or retry if busy.")
}

pub fn snapshot(
    store: &Store,
    pinned: Option<&Scope>,
    args: &Value,
) -> Result<Value, &'static str> {
    let args = args.as_object().ok_or("arguments must be an object")?;
    if args
        .keys()
        .any(|k| !matches!(k.as_str(), "project" | "after" | "query"))
    {
        return Err("unexpected argument");
    }
    let scope = project(pinned, args.get("project"))?.ok_or("enter a project slug")?;
    let query = args
        .get("query")
        .map(|x| x.as_str().ok_or("query must be text"))
        .transpose()?
        .unwrap_or("");
    if query.len() > 512 {
        return Err("query exceeds 512 UTF-8 bytes");
    }
    let after = args
        .get("after")
        .map(|x| {
            x.as_str()
                .filter(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("invalid page cursor")
        })
        .transpose()?
        .unwrap_or(0);
    store
        .snapshot(&scope, after, query)
        .map_err(|_| "Cannot read the local store. Check permissions or retry if busy.")
}
