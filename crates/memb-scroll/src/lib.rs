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
pub const EVIDENCE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("scroll_evidence_staged_v1");
pub const IDLE_SWEEP_MIN_MESSAGES: usize = 45;
pub const DEFAULT_IDLE_MS: u64 = 24 * 60 * 60 * 1000;
pub const MAX_CHUNK_LIST: usize = 100;

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

/// Tunable scroll limits (config.toml `[scroll]`); `Default` is the shipped behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollLimits {
    pub max_messages: usize,
    pub retain_tail: usize,
    pub min_prompt_chars: usize,
    pub dedup_reset_chunks: u32,
}

impl Default for ScrollLimits {
    fn default() -> Self {
        Self {
            max_messages: SCROLL_MAX_MESSAGES,
            retain_tail: SCROLL_RETAIN_TAIL,
            min_prompt_chars: MIN_PROMPT_CHARS_FOR_RETRIEVAL,
            dedup_reset_chunks: DEFAULT_DEDUP_RESET_CHUNKS,
        }
    }
}

pub fn should_retrieve_for_prompt(latest_user_msg: &str) -> bool {
    should_retrieve_for_prompt_with(latest_user_msg, &ScrollLimits::default())
}

pub fn should_retrieve_for_prompt_with(latest_user_msg: &str, limits: &ScrollLimits) -> bool {
    latest_user_msg.trim().chars().count() >= limits.min_prompt_chars
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
    capture_opt_in: bool,
) -> Result<Option<ScrollChunk>, String> {
    append_message_with(db, session_id, cli, role, content, capture_opt_in, &ScrollLimits::default())
}

pub fn append_message_with(
    db: &Database,
    session_id: &str,
    cli: &str,
    role: Role,
    content: String,
    capture_opt_in: bool,
    limits: &ScrollLimits,
) -> Result<Option<ScrollChunk>, String> {
    if limits.retain_tail == 0 || limits.retain_tail >= limits.max_messages {
        return Err("invalid scroll limits".into());
    }
    if !capture_opt_in || is_memory_disabled(None, &[]) {
        return Err("scroll capture is disabled".into());
    }
    for value in [content.as_str(), session_id, cli] {
        memb_core::reject_secrets(value).map_err(|_| "potential secret rejected".to_string())?;
    }
    let ts = now_ms();
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let rolled_chunk = {
        let mut scrolls = w.open_table(SCROLLS_TABLE).map_err(|e| e.to_string())?;
        let mut chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let mut meta_tbl = w.open_table(SESSION_META_TABLE).map_err(|e| e.to_string())?;

        let mut scroll = scrolls
            .get(session_id)
            .map_err(|e| e.to_string())?
            .map(|v| serde_json::from_slice::<ChatScroll>(v.value()).map_err(|e| e.to_string()))
            .transpose()?
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
            .map(|v| serde_json::from_slice::<SessionMeta>(v.value()).map_err(|e| e.to_string()))
            .transpose()?
            .unwrap_or_default();
        meta.last_active_ms = ts;

        let emitted = if scroll.messages.len() >= limits.max_messages {
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

            let keep_from = scroll.messages.len().saturating_sub(limits.retain_tail);
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

/// Flush at most 100 idle sessions atomically. The last ten messages overlap the chunk.
/// Strictly more than `idle_ms` must have elapsed, and 45 messages are required.
pub fn sweep_idle_scrolls(db: &Database, now_ms: u64, idle_ms: u64) -> Result<Vec<ScrollChunk>, String> {
    sweep_idle_scrolls_with(db, now_ms, idle_ms, IDLE_SWEEP_MIN_MESSAGES, SCROLL_RETAIN_TAIL)
}

/// `min_messages` must exceed `retain_tail`, otherwise nothing could be chunked.
pub fn sweep_idle_scrolls_with(
    db: &Database,
    now_ms: u64,
    idle_ms: u64,
    min_messages: usize,
    retain_tail: usize,
) -> Result<Vec<ScrollChunk>, String> {
    if retain_tail == 0 || min_messages <= retain_tail {
        return Err("invalid sweep limits".into());
    }
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let emitted = {
        let mut scrolls = w.open_table(SCROLLS_TABLE).map_err(|e| e.to_string())?;
        let mut chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let mut metas = w.open_table(SESSION_META_TABLE).map_err(|e| e.to_string())?;
        let mut candidates = Vec::new();
        for entry in scrolls.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let scroll: ChatScroll = serde_json::from_slice(value.value()).map_err(|e| e.to_string())?;
            if scroll.session_id != key.value() { return Err("scroll session mismatch".into()); }
            if scroll.messages.len() >= min_messages
                && now_ms.checked_sub(scroll.updated_at_ms).is_some_and(|age| age > idle_ms) {
                candidates.push(scroll);
                if candidates.len() == MAX_CHUNK_LIST { break; }
            }
        }
        let mut result = Vec::with_capacity(candidates.len());
        for mut scroll in candidates {
            let session = scroll.session_id.clone();
            let mut meta = metas.get(session.as_str()).map_err(|e| e.to_string())?
                .map(|v| serde_json::from_slice::<SessionMeta>(v.value()).map_err(|e| e.to_string()))
                .transpose()?.ok_or("missing session metadata")?;
            let index = meta.total_chunks;
            let next = index.checked_add(1).ok_or("chunk counter overflow")?;
            let chunk_id = format!("chk_{session}_{index:04}");
            if chunks.get(chunk_id.as_str()).map_err(|e| e.to_string())?.is_some() {
                return Err("chunk already exists".into());
            }
            let chunk = ScrollChunk { chunk_id: chunk_id.clone(), session_id: session.clone(), chunk_index: index,
                messages: scroll.messages.clone(), summary: None, created_at_ms: now_ms, curated: false };
            scroll.messages = scroll.messages.split_off(scroll.messages.len() - retain_tail);
            meta.total_chunks = next;
            let cb = serde_json::to_vec(&chunk).map_err(|e| e.to_string())?;
            let sb = serde_json::to_vec(&scroll).map_err(|e| e.to_string())?;
            let mb = serde_json::to_vec(&meta).map_err(|e| e.to_string())?;
            chunks.insert(chunk_id.as_str(), cb.as_slice()).map_err(|e| e.to_string())?;
            scrolls.insert(session.as_str(), sb.as_slice()).map_err(|e| e.to_string())?;
            metas.insert(session.as_str(), mb.as_slice()).map_err(|e| e.to_string())?;
            result.push(chunk);
        }
        result
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(emitted)
}

/// Returns uncurated, not-yet-staged chunks, capped at 100.
pub fn list_uncurated_chunks(db: &Database, limit: usize) -> Result<Vec<ScrollChunk>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let chunks = match r.open_table(CHUNKS_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let staged = match r.open_table(EVIDENCE_TABLE) {
        Ok(table) => Some(table),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.to_string()),
    };
    let mut result = Vec::new();
    if limit == 0 { return Ok(result); }
    for entry in chunks.iter().map_err(|e| e.to_string())? {
        let (key, value) = entry.map_err(|e| e.to_string())?;
        let chunk: ScrollChunk = serde_json::from_slice(value.value()).map_err(|e| e.to_string())?;
        if chunk.chunk_id != key.value() { return Err("chunk id mismatch".into()); }
        if !chunk.curated && match &staged { Some(t) => t.get(key.value()).map_err(|e| e.to_string())?.is_none(), None => true } {
            result.push(chunk);
            if result.len() == limit.min(MAX_CHUNK_LIST) { break; }
        }
    }
    Ok(result)
}

/// Atomically records that the persisted chunk has been staged as evidence.
/// Does not curate or remove the chunk. False means it was already staged.
pub fn stage_chunk_evidence(db: &Database, chunk_id: &str) -> Result<bool, String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let inserted = {
        let chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let mut staged = w.open_table(EVIDENCE_TABLE).map_err(|e| e.to_string())?;
        let raw = chunks.get(chunk_id).map_err(|e| e.to_string())?.ok_or("chunk not found")?;
        let chunk: ScrollChunk = serde_json::from_slice(raw.value()).map_err(|e| e.to_string())?;
        if chunk.chunk_id != chunk_id { return Err("chunk id mismatch".into()); }
        if staged.get(chunk_id).map_err(|e| e.to_string())?.is_some() { false }
        else { staged.insert(chunk_id, b"1".as_slice()).map_err(|e| e.to_string())?; true }
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(inserted)
}

/// Read-only view of one live (not yet chunked) scroll. Used by hooks to avoid
/// recording the same turn twice when a host fires its stop event repeatedly.
pub fn read_scroll(db: &Database, session_id: &str) -> Result<Option<ChatScroll>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(SCROLLS_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let Some(raw) = table.get(session_id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    serde_json::from_slice(raw.value()).map(Some).map_err(|e| e.to_string())
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
            .map(|v| serde_json::from_slice::<SessionMeta>(v.value()).map_err(|e| e.to_string()))
            .transpose()?
            .map(|m| m.total_chunks)
            .unwrap_or(0);
        let epoch = total_chunks / divisor;
        let key = format!("{}:{session_id}:{epoch}:{node_id}", session_id.len());
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
        assert!(!should_retrieve_for_prompt(&"a".repeat(99)));
        assert!(should_retrieve_for_prompt(&"a".repeat(100)));
        assert!(!should_retrieve_for_prompt(&format!(" {} ", "a".repeat(99))));
        assert!(should_retrieve_for_prompt(&"é".repeat(100)));
    }

    #[test]
    fn custom_limits_change_rollover_retrieval_gate_and_idle_sweep() {
        let p = std::env::temp_dir().join(format!("memb_scroll_limits_{}.redb", now_ms()));
        let db = Database::create(&p).unwrap();
        let limits = ScrollLimits { max_messages: 20, retain_tail: 4, min_prompt_chars: 5, dedup_reset_chunks: 3 };
        assert!(should_retrieve_for_prompt_with("12345", &limits));
        assert!(!should_retrieve_for_prompt_with("1234", &limits));
        assert!(!should_retrieve_for_prompt("12345")); // the default gate is still 100
        let mut chunks = Vec::new();
        for i in 0..40 {
            if let Some(c) = append_message_with(&db, "s", "cc", Role::User, format!("m{i}"), true, &limits).unwrap() {
                chunks.push(c);
            }
        }
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.messages.len() == 20));
        assert_eq!(read_scroll(&db, "s").unwrap().unwrap().messages.len(), 40 - 2 * 16);
        // Invalid combinations are rejected instead of misbehaving.
        for bad in [ScrollLimits { retain_tail: 0, ..limits.clone() }, ScrollLimits { retain_tail: 20, ..limits.clone() }] {
            assert!(append_message_with(&db, "x", "cc", Role::User, "hi".into(), true, &bad).is_err());
        }
        // Sweep: threshold and retained tail are parameters; 30 messages idle since `old`.
        for i in 0..30 {
            append_message_with(&db, "idle", "cc", Role::User, format!("i{i}"), true, &ScrollLimits::default()).unwrap();
        }
        let now = read_scroll(&db, "idle").unwrap().unwrap().updated_at_ms + 10_000;
        assert!(sweep_idle_scrolls_with(&db, now, 5_000, 31, 3).unwrap().is_empty()); // below threshold
        assert!(sweep_idle_scrolls_with(&db, now, 20_000, 30, 3).unwrap().is_empty()); // not idle long enough
        assert!(sweep_idle_scrolls_with(&db, now, 5_000, 3, 3).is_err()); // threshold must exceed the tail
        let swept = sweep_idle_scrolls_with(&db, now, 5_000, 30, 3).unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].messages.len(), 30);
        assert_eq!(read_scroll(&db, "idle").unwrap().unwrap().messages.len(), 3);
        drop(db);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn rollover_at_90_retains_last_10_and_resets_dedup_at_7_chunks() {
        let p = std::env::temp_dir().join(format!("memb_scroll_{}.redb", now_ms()));
        let db = Database::create(&p).unwrap();
        let mut chunks = 0;
        assert!(append_message(&db, "s1", "cc", Role::User, "disabled".into(), false).is_err());
        assert!(append_message(&db, "s1", "cc", Role::User, "password=123456789".into(), true).is_err());
        assert!(should_inject_and_mark(&db, "s1", 42, 7).unwrap());
        for i in 0..570 {
            let emitted = append_message(&db, "s1", "cc", Role::User, format!("msg {i}"), true).unwrap();
            if let Some(chunk) = emitted {
                chunks += 1;
                assert_eq!(chunk.messages.len(), 90);
                if chunks < 7 { assert!(!should_inject_and_mark(&db, "s1", 42, 7).unwrap()); }
            }
            if i == 88 { assert_eq!(chunks, 0); }
        }
        assert_eq!(chunks, 7);
        assert!(should_inject_and_mark(&db, "s1", 42, 7).unwrap());
        assert!(!should_inject_and_mark(&db, "s1", 42, 7).unwrap());
        let page = read_chunk(&db, "chk_s1_0000", 85, 100).unwrap().unwrap();
        assert_eq!(page["messages"].as_array().unwrap().len(), 5);
        let r = db.begin_read().unwrap();
        let table = r.open_table(SCROLLS_TABLE).unwrap();
        let raw = table.get("s1").unwrap().unwrap();
        let scroll: ChatScroll = serde_json::from_slice(raw.value()).unwrap();
        assert_eq!(scroll.messages.len(), 10);
        assert_eq!(scroll.messages[0].content, "msg 560");
        drop(raw); drop(table); drop(r); drop(db);
        std::fs::remove_file(&p).unwrap();
    }
}
