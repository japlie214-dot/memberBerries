use memb_store::Store;
use serde_json::Value;

pub const HTML: &str = include_str!("viewer.html");

pub fn snapshot(store: &Store, query: Option<&str>, after: Option<u64>) -> Result<Value, String> {
    store.snapshot(query, after)
}

pub fn graph(store: &Store, include_inactive: bool) -> Result<Value, String> {
    store.graph(include_inactive)
}
