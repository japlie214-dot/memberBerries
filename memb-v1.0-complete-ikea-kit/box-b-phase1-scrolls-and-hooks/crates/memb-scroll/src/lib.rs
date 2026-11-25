use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCROLL_MAX_MESSAGES: usize = 90;
pub const SCROLL_RETAIN_TAIL: usize = 10;
pub const MIN_PROMPT_CHARS_FOR_RETRIEVAL: usize = 100;
pub const DEFAULT_DEDUP_RESET_CHUNKS: u32 = 7;

pub const SCROLLS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("scrolls_v1");
pub const CHUNKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("scroll_chunks_v1");
pub const SESSION_META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("session_meta_v1");
pub const INJECTIONS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("session_injections_v1");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollMessage {
    pub role: Role,
    pub content: String,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatScroll {
    pub session_id: String,
    pub cli: String,
    pub messages: Vec<ScrollMessage>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollChunk {
    pub chunk_id: String,
    pub session_id: String,
    pub chunk_index: u32,
    pub messages: Vec<ScrollMessage>,
    pub summary: Option<String>,
    pub created_at_ms: u64,
    pub curated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub total_chunks: u32,
    pub last_active_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn should_retrieve_for_prompt(latest_user_msg: &str) -> bool {
    latest_user_msg.trim().chars().count() >= MIN_PROMPT_CHARS_FOR_RETRIEVAL
}

pub fn is_memory_disabled(cwd: Option<&str>, ignore_dirs: &[String]) -> bool {
    if std::env::var("MEMB_NO_MEMORY").ok().as_deref() == Some("1")
        || std::env::var("MEMB_INTERNAL").ok().as_deref() == Some("1")
    {
        return true;
    }
    if let Some(dir) = cwd {
        let norm = dir.replace('\\', "/").to_lowercase();
        for pat in ignore_dirs {
            let needle = pat.replace('\\', "/").replace('*', "").to_lowercase();
            if !needle.is_empty() && norm.contains(&needle) {
                return true;
            }
        }
    }
    false
}

pub fn append_message(
    db: &Database,
    session_id: &str,
    cli: &str,
    role: Role,
    content: String,
) -> Result<Option<ScrollChunk>, String> {
    let ts = now_ms();
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let rolled_chunk = {
        let mut scrolls = w.open_table(SCROLLS_TABLE).map_err(|e| e.to_string())?;
        let mut chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let mut meta_tbl = w.open_table(SESSION_META_TABLE).map_err(|e| e.to_string())?;

        let mut scroll = scrolls
            .get(session_id)
            .map_err(|e| e.to_string())?
            .and_then(|v| serde_json::from_slice::<ChatScroll>(v.value()).ok())
            .unwrap_or_else(|| ChatScroll {
                session_id: session_id.to_string(),
                cli: cli.to_string(),
                messages: Vec::new(),
                updated_at_ms: ts,
            });

        scroll.messages.push(ScrollMessage { role, content, ts_ms: ts });
        scroll.updated_at_ms = ts;

        let mut meta = meta_tbl
            .get(session_id)
            .map_err(|e| e.to_string())?
            .and_then(|v| serde_json::from_slice::<SessionMeta>(v.value()).ok())
            .unwrap_or_default();
        meta.last_active_ms = ts;

        let emitted = if scroll.messages.len() >= SCROLL_MAX_MESSAGES {
            let chunk_index = meta.total_chunks;
            let chunk_id = format!("chk_{session_id}_{chunk_index:04}");
            let chunk = ScrollChunk {
                chunk_id: chunk_id.clone(),
                session_id: session_id.to_string(),
                chunk_index,
                messages: scroll.messages.clone(),
                summary: None,
                created_at_ms: ts,
                curated: false,
            };
            let cbytes = serde_json::to_vec(&chunk).map_err(|e| e.to_string())?;
            chunks.insert(chunk_id.as_str(), cbytes.as_slice()).map_err(|e| e.to_string())?;

            let keep_from = scroll.messages.len().saturating_sub(SCROLL_RETAIN_TAIL);
            scroll.messages = scroll.messages.split_off(keep_from);
            meta.total_chunks += 1;
            Some(chunk)
        } else {
            None
        };

        let sbytes = serde_json::to_vec(&scroll).map_err(|e| e.to_string())?;
        let mbytes = serde_json::to_vec(&meta).map_err(|e| e.to_string())?;
        scrolls.insert(session_id, sbytes.as_slice()).map_err(|e| e.to_string())?;
        meta_tbl.insert(session_id, mbytes.as_slice()).map_err(|e| e.to_string())?;
        emitted
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(rolled_chunk)
}

pub fn should_inject_and_mark(
    db: &Database,
    session_id: &str,
    node_id: u64,
    reset_every_n_chunks: u32,
) -> Result<bool, String> {
    let divisor = reset_every_n_chunks.max(1);
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let should_inject = {
        let meta_tbl = w.open_table(SESSION_META_TABLE).map_err(|e| e.to_string())?;
        let mut inj = w.open_table(INJECTIONS_TABLE).map_err(|e| e.to_string())?;
        let total_chunks = meta_tbl
            .get(session_id)
            .map_err(|e| e.to_string())?
            .and_then(|v| serde_json::from_slice::<SessionMeta>(v.value()).ok())
            .map(|m| m.total_chunks)
            .unwrap_or(0);
        let epoch = total_chunks / divisor;
        let key = format!("{session_id}:{epoch}:{node_id}");
        if inj.get(key.as_str()).map_err(|e| e.to_string())?.is_some() {
            false
        } else {
            inj.insert(key.as_str(), b"1".as_slice()).map_err(|e| e.to_string())?;
            true
        }
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(should_inject)
}

pub fn read_chunk(
    db: &Database,
    chunk_id: &str,
    offset: usize,
    limit: usize,
) -> Result<Option<serde_json::Value>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = r.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
    let Some(raw) = table.get(chunk_id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let chunk: ScrollChunk = serde_json::from_slice(raw.value()).map_err(|e| e.to_string())?;
    let slice: Vec<&ScrollMessage> = chunk.messages.iter().skip(offset).take(limit.min(50)).collect();
    Ok(Some(serde_json::json!({
        "chunk_id": chunk.chunk_id,
        "session_id": chunk.session_id,
        "chunk_index": chunk.chunk_index,
        "total_messages": chunk.messages.len(),
        "offset": offset,
        "summary": chunk.summary,
        "messages": slice
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_gate_enforces_100_chars() {
        assert!(!should_retrieve_for_prompt("short prompt"));
        assert!(should_retrieve_for_prompt(&"a".repeat(100)));
    }

    #[test]
    fn rollover_at_90_retains_last_10_and_resets_dedup_at_7_chunks() {
        let p = std::env::temp_dir().join(format!("memb_scroll_{}.redb", now_ms()));
        let db = Database::create(&p).unwrap();
        let mut chunks = 0;
        for i in 0..90 {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            if append_message(&db, "s1", "cc", role, format!("msg {i}")).unwrap().is_some() {
                chunks += 1;
            }
        }
        assert_eq!(chunks, 1);
        assert!(should_inject_and_mark(&db, "s1", 42, 7).unwrap());
        assert!(!should_inject_and_mark(&db, "s1", 42, 7).unwrap());
        let _ = std::fs::remove_file(&p);
    }
}
