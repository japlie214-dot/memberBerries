use memb_store::Store;
use serde_json::Value;

pub const HTML: &str = include_str!("viewer.html");

pub fn graph(store: &Store, args: &Value) -> Result<Value, &'static str> {
    let args = args.as_object().ok_or("invalid arguments")?;
    if !args.is_empty() {
        return Err("unexpected argument");
    }
    store
        .graph(false)
        .map_err(|_| "Cannot read the local store.")
}

pub fn snapshot(store: &Store, args: &Value) -> Result<Value, &'static str> {
    let args = args.as_object().ok_or("invalid arguments")?;
    if args
        .keys()
        .any(|k| !matches!(k.as_str(), "query" | "after"))
    {
        return Err("unexpected argument");
    }
    let query = args
        .get("query")
        .map(|v| v.as_str().ok_or("invalid query"))
        .transpose()?;
    if query.is_some_and(|q| q.len() > 512) {
        return Err("query exceeds 512 bytes");
    }
    let after = args
        .get("after")
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("invalid page cursor")
        })
        .transpose()?;
    let mut page = store
        .snapshot(query, after)
        .map_err(|_| "Cannot read the local store.")?;
    if let Some(notes) = page.get_mut("notes").and_then(Value::as_array_mut) {
        for note in notes {
            if let Some(id) = note.get("id").and_then(Value::as_u64) {
                note["id"] = Value::String(id.to_string());
            }
            if let Some(parents) = note.get_mut("parents").and_then(Value::as_array_mut) {
                for parent in parents {
                    if let Some(id) = parent.as_u64() {
                        *parent = Value::String(id.to_string());
                    }
                }
            }
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshots_allow_bounded_navigation_and_search() {
        let root = std::env::temp_dir().join(format!(
            "memb-viewer-page-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&root).unwrap();
        for i in 0..13 {
            let draft = serde_json::from_value(
                json!({"title":format!("Note {i}"),"desc":"Synthetic", "kind":"topic"}),
            )
            .unwrap();
            store.remember(draft).unwrap();
        }
        let first = snapshot(&store, &json!({})).unwrap();
        assert_eq!(first["notes"].as_array().unwrap().len(), 12);
        assert_eq!(first["notes"][0]["id"], "1");
        let second = snapshot(&store, &json!({"after":first["next"]})).unwrap();
        assert_eq!(second["notes"].as_array().unwrap().len(), 1);
        assert_eq!(second["notes"][0]["id"], "13");
        assert!(second["next"].is_null());
        let search = snapshot(&store, &json!({"query":"Note 12"})).unwrap();
        assert_eq!(search["notes"][0]["id"], "13");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
