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
    /// Layer 1 summary, written by the configured model after the chunk exists.
    pub summary: Option<String>,
    pub created_at_ms: u64,
    pub curated: bool,
    /// True when the summarizer saw an explicit user directive ("always use", ...).
    #[serde(default)]
    pub explicit_signal: Option<bool>,
    /// Quantized embedding of `summary`, tagged with the embedding model's identity.
    #[serde(default)]
    pub summary_vec: Option<ChunkVec>,
}

/// An i8-quantized vector plus the identity of the model that made it. Vectors with
/// different tags are never compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkVec {
    pub model: String,
    pub scale: f32,
    pub data: Vec<i8>,
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
    let prompt = latest_user_msg.trim();
    !is_slash_command(prompt) && prompt.chars().count() >= limits.min_prompt_chars
}

/// A slash command (`^/[A-Za-z0-9_-]+`, such as `/help` or `/review the diff ...`) is
/// an instruction to the host, not a question: it never triggers retrieval, however long.
/// A path such as `/usr/bin` or `/ not a command` is not one only if it does not start
/// with a slash followed directly by a letter, digit, `_` or `-`.
pub fn is_slash_command(prompt: &str) -> bool {
    let mut chars = prompt.chars();
    chars.next() == Some('/') && chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
                explicit_signal: None,
                summary_vec: None,
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
                messages: scroll.messages.clone(), summary: None, created_at_ms: now_ms, curated: false, explicit_signal: None, summary_vec: None };
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
        else { staged.insert(chunk_id, now_ms().to_string().as_bytes()).map_err(|e| e.to_string())?; true }
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(inserted)
}

/// Staged, not yet curated chunks in chunk-id order (not time order), capped at `limit` (at most 100).
pub fn list_staged_chunks(db: &Database, limit: usize) -> Result<Vec<ScrollChunk>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let (chunks, staged) = match (r.open_table(CHUNKS_TABLE), r.open_table(EVIDENCE_TABLE)) {
        (Ok(c), Ok(s)) => (c, s),
        (Err(redb::TableError::TableDoesNotExist(_)), _) | (_, Err(redb::TableError::TableDoesNotExist(_))) => return Ok(Vec::new()),
        (Err(e), _) | (_, Err(e)) => return Err(e.to_string()),
    };
    let mut result = Vec::new();
    for entry in staged.iter().map_err(|e| e.to_string())? {
        if result.len() >= limit.min(MAX_CHUNK_LIST) { break; }
        let (key, _) = entry.map_err(|e| e.to_string())?;
        let Some(raw) = chunks.get(key.value()).map_err(|e| e.to_string())? else { continue };
        let chunk: ScrollChunk = serde_json::from_slice(raw.value()).map_err(|e| e.to_string())?;
        if chunk.chunk_id != key.value() { return Err("chunk id mismatch".into()); }
        if !chunk.curated { result.push(chunk); }
    }
    Ok(result)
}

/// Marks chunks as curated (handled) and removes their staging rows. The raw chunk
/// text is kept; it is simply no longer offered for curation. Unknown ids are skipped.
pub fn mark_chunks_curated(db: &Database, chunk_ids: &[String]) -> Result<usize, String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let mut done = 0;
    {
        let mut chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let mut staged = w.open_table(EVIDENCE_TABLE).map_err(|e| e.to_string())?;
        for id in chunk_ids {
            let raw = chunks.get(id.as_str()).map_err(|e| e.to_string())?.map(|v| v.value().to_vec());
            let Some(raw) = raw else { continue };
            let mut chunk: ScrollChunk = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
            chunk.curated = true;
            let bytes = serde_json::to_vec(&chunk).map_err(|e| e.to_string())?;
            chunks.insert(id.as_str(), bytes.as_slice()).map_err(|e| e.to_string())?;
            staged.remove(id.as_str()).map_err(|e| e.to_string())?;
            done += 1;
        }
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(done)
}

/// Enforces the staging time limit: chunks staged longer than `ttl_ms` ago (and not
/// curated) leave staging and are marked curated, so they are no longer offered. Raw
/// chunk text is not deleted. A legacy staging row without a timestamp is aged from
/// the chunk's creation time. Returns how many chunks expired.
pub fn expire_staged_chunks(db: &Database, now_ms: u64, ttl_ms: u64) -> Result<usize, String> {
    let expired: Vec<String> = {
        let r = db.begin_read().map_err(|e| e.to_string())?;
        let (chunks, staged) = match (r.open_table(CHUNKS_TABLE), r.open_table(EVIDENCE_TABLE)) {
            (Ok(c), Ok(s)) => (c, s),
            (Err(redb::TableError::TableDoesNotExist(_)), _) | (_, Err(redb::TableError::TableDoesNotExist(_))) => return Ok(0),
            (Err(e), _) | (_, Err(e)) => return Err(e.to_string()),
        };
        let mut out = Vec::new();
        for entry in staged.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let staged_at = std::str::from_utf8(value.value()).ok().and_then(|t| t.parse::<u64>().ok()).filter(|&t| t > 1);
            let at = match staged_at {
                Some(t) => t,
                None => match chunks.get(key.value()).map_err(|e| e.to_string())? {
                    Some(raw) => serde_json::from_slice::<ScrollChunk>(raw.value()).map_err(|e| e.to_string())?.created_at_ms,
                    None => 0,
                },
            };
            if now_ms.saturating_sub(at) > ttl_ms { out.push(key.value().to_string()); }
        }
        out
    };
    if expired.is_empty() { return Ok(0); }
    let w = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let mut staged = w.open_table(EVIDENCE_TABLE).map_err(|e| e.to_string())?;
        for id in &expired {
            let raw = chunks.get(id.as_str()).map_err(|e| e.to_string())?.map(|v| v.value().to_vec());
            if let Some(raw) = raw {
                let mut chunk: ScrollChunk = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
                chunk.curated = true;
                let bytes = serde_json::to_vec(&chunk).map_err(|e| e.to_string())?;
                chunks.insert(id.as_str(), bytes.as_slice()).map_err(|e| e.to_string())?;
            }
            staged.remove(id.as_str()).map_err(|e| e.to_string())?;
        }
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(expired.len())
}

/// Chunks that have no summary yet, in chunk-id order, at most `limit` (at most 100).
pub fn list_unsummarized_chunks(db: &Database, limit: usize) -> Result<Vec<ScrollChunk>, String> {
    list_chunks_where(db, limit, |c| c.summary.is_none())
}

/// Chunks that have a summary but no vector yet (for example the embedding backend was
/// off when they were summarized).
pub fn list_unembedded_chunks(db: &Database, limit: usize) -> Result<Vec<ScrollChunk>, String> {
    list_chunks_where(db, limit, |c| c.summary.is_some() && c.summary_vec.is_none())
}

fn list_chunks_where(
    db: &Database,
    limit: usize,
    keep: impl Fn(&ScrollChunk) -> bool,
) -> Result<Vec<ScrollChunk>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let chunks = match r.open_table(CHUNKS_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = Vec::new();
    for entry in chunks.iter().map_err(|e| e.to_string())? {
        if out.len() >= limit.min(MAX_CHUNK_LIST) { break; }
        let (key, value) = entry.map_err(|e| e.to_string())?;
        let chunk: ScrollChunk = serde_json::from_slice(value.value()).map_err(|e| e.to_string())?;
        if chunk.chunk_id != key.value() { return Err("chunk id mismatch".into()); }
        if keep(&chunk) { out.push(chunk); }
    }
    Ok(out)
}

/// Stores a summary (and the explicit-directive flag) on an existing chunk. A chunk that
/// already has a summary keeps it: false means nothing was written.
pub fn set_chunk_summary(db: &Database, chunk_id: &str, summary: &str, explicit_signal: bool) -> Result<bool, String> {
    update_chunk(db, chunk_id, |c| {
        if c.summary.is_some() { return false; }
        c.summary = Some(summary.to_string());
        c.explicit_signal = Some(explicit_signal);
        true
    })
}

/// Stores the summary vector on a chunk that has a summary but no vector.
pub fn set_chunk_vector(db: &Database, chunk_id: &str, vector: ChunkVec) -> Result<bool, String> {
    update_chunk(db, chunk_id, |c| {
        if c.summary.is_none() || c.summary_vec.is_some() { return false; }
        c.summary_vec = Some(vector);
        true
    })
}

fn update_chunk(db: &Database, chunk_id: &str, change: impl FnOnce(&mut ScrollChunk) -> bool) -> Result<bool, String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let changed = {
        let mut chunks = w.open_table(CHUNKS_TABLE).map_err(|e| e.to_string())?;
        let raw = chunks.get(chunk_id).map_err(|e| e.to_string())?.map(|v| v.value().to_vec()).ok_or("chunk not found")?;
        let mut chunk: ScrollChunk = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        if chunk.chunk_id != chunk_id { return Err("chunk id mismatch".into()); }
        let changed = change(&mut chunk);
        if changed {
            let bytes = serde_json::to_vec(&chunk).map_err(|e| e.to_string())?;
            chunks.insert(chunk_id, bytes.as_slice()).map_err(|e| e.to_string())?;
        }
        changed
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(changed)
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
    should_inject_and_mark_version(db, session_id, node_id, reset_every_n_chunks, 0)
}

/// Like [`should_inject_and_mark`], with the note's last-update time: a note that was
/// updated after it was injected in this epoch is injected again (the model saw the old
/// text), once per update.
pub fn should_inject_and_mark_version(
    db: &Database,
    session_id: &str,
    node_id: u64,
    reset_every_n_chunks: u32,
    note_version: u64,
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
        // The value is the note version last injected (older entries hold a marker byte,
        // read as version 0).
        let seen = inj
            .get(key.as_str())
            .map_err(|e| e.to_string())?
            .map(|v| <[u8; 8]>::try_from(v.value()).map(u64::from_le_bytes).unwrap_or(0));
        match seen {
            Some(version) if version >= note_version => false,
            _ => {
                inj.insert(key.as_str(), note_version.to_le_bytes().as_slice()).map_err(|e| e.to_string())?;
                true
            }
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
    fn idle_sweep_leaves_a_scroll_of_44_messages_and_chunks_one_of_45() {
        let path = std::env::temp_dir().join(format!("memb_scroll_idle_floor_{}.redb", now_ms()));
        let db = Database::create(&path).unwrap();
        for i in 0..44 {
            assert!(append_message(&db, "small", "cc", Role::User, format!("m{i}"), true).unwrap().is_none());
        }
        for i in 0..45 {
            assert!(append_message(&db, "big", "cc", Role::User, format!("m{i}"), true).unwrap().is_none());
        }
        let later = now_ms() + DEFAULT_IDLE_MS + 1000;
        // Not idle for a day yet: nothing, whatever the size.
        assert!(sweep_idle_scrolls(&db, now_ms(), DEFAULT_IDLE_MS).unwrap().is_empty());
        let swept = sweep_idle_scrolls(&db, later, DEFAULT_IDLE_MS).unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!((swept[0].session_id.as_str(), swept[0].messages.len()), ("big", 45));
        assert_eq!(read_scroll(&db, "small").unwrap().unwrap().messages.len(), 44);
        assert_eq!(read_scroll(&db, "big").unwrap().unwrap().messages.len(), SCROLL_RETAIN_TAIL);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slash_commands_never_trigger_retrieval_however_long() {
        assert!(!should_retrieve_for_prompt("/help"));
        let long_command = format!("/review {}", "check the diff carefully ".repeat(8));
        assert!(long_command.chars().count() > 100);
        assert!(!should_retrieve_for_prompt(&long_command));
        assert!(!should_retrieve_for_prompt(&format!("  /compact {}", "x".repeat(150))));
        assert!(!should_retrieve_for_prompt(&format!("/my_skill-2 {}", "x".repeat(150))));
        // Not commands: a sentence that merely contains a slash, a lone slash, a slash then space.
        assert!(should_retrieve_for_prompt(&format!("how does a/b work {}", "x".repeat(100))));
        assert!(should_retrieve_for_prompt(&format!("/ {}", "x".repeat(120))));
        assert!(should_retrieve_for_prompt(&format!("what is /help {}", "x".repeat(100))));
    }

    #[test]
    fn an_updated_note_is_injected_again_in_the_same_epoch_once_per_update() {
        let path = std::env::temp_dir().join(format!("memb_scroll_reinject_{}.redb", now_ms()));
        let db = Database::create(&path).unwrap();
        assert!(should_inject_and_mark_version(&db, "s1", 7, 7, 100).unwrap());
        assert!(!should_inject_and_mark_version(&db, "s1", 7, 7, 100).unwrap(), "unchanged: not again");
        assert!(should_inject_and_mark_version(&db, "s1", 7, 7, 101).unwrap(), "updated: again");
        assert!(!should_inject_and_mark_version(&db, "s1", 7, 7, 101).unwrap(), "once per update");
        assert!(!should_inject_and_mark_version(&db, "s1", 7, 7, 90).unwrap(), "an older version never re-injects");
        assert!(should_inject_and_mark_version(&db, "s1", 8, 7, 5).unwrap(), "another note is independent");
        drop(db);
        let _ = std::fs::remove_file(path);
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
    fn staged_evidence_is_listed_curated_and_expires_after_the_limit() {
        let p = std::env::temp_dir().join(format!("memb_scroll_stage_{}.redb", now_ms()));
        let db = Database::create(&p).unwrap();
        let limits = ScrollLimits { max_messages: 10, retain_tail: 2, min_prompt_chars: 5, dedup_reset_chunks: 3 };
        let mut ids = Vec::new();
        for i in 0..30 {
            if let Some(c) = append_message_with(&db, "s", "cc", Role::User, format!("m{i}"), true, &limits).unwrap() {
                ids.push(c.chunk_id);
            }
        }
        assert_eq!(ids.len(), 3);
        assert!(list_staged_chunks(&db, 10).unwrap().is_empty(), "nothing is staged yet");
        for id in &ids { assert!(stage_chunk_evidence(&db, id).unwrap()); }
        assert!(!stage_chunk_evidence(&db, &ids[0]).unwrap(), "staging twice is a no-op");
        assert_eq!(list_staged_chunks(&db, 10).unwrap().len(), 3);
        assert_eq!(list_staged_chunks(&db, 2).unwrap().len(), 2);
        assert!(list_uncurated_chunks(&db, 10).unwrap().is_empty(), "staged chunks are not offered for staging again");

        // Curating one removes it from staging; the raw chunk stays readable.
        assert_eq!(mark_chunks_curated(&db, &[ids[0].clone(), "chk_missing_0000".into()]).unwrap(), 1);
        assert_eq!(list_staged_chunks(&db, 10).unwrap().len(), 2);
        assert!(read_chunk(&db, &ids[0], 0, 1).unwrap().is_some());

        // Expiry: not yet at the limit, then past it. Expired chunks are not offered again.
        let day = 24 * 60 * 60 * 1000;
        let now = now_ms();
        assert_eq!(expire_staged_chunks(&db, now + 29 * day, 30 * day).unwrap(), 0);
        assert_eq!(expire_staged_chunks(&db, now + 31 * day, 30 * day).unwrap(), 2);
        assert!(list_staged_chunks(&db, 10).unwrap().is_empty());
        assert!(list_uncurated_chunks(&db, 10).unwrap().is_empty());
        assert!(read_chunk(&db, &ids[1], 0, 1).unwrap().is_some(), "expiry never deletes raw text");
        assert_eq!(expire_staged_chunks(&db, now + 60 * day, 30 * day).unwrap(), 0);

        // A legacy staging row (no timestamp) is aged from the chunk's creation time.
        let legacy = {
            let w = db.begin_write().unwrap();
            let id = "chk_legacy_0000";
            {
                let chunk = ScrollChunk { chunk_id: id.into(), session_id: "legacy".into(), chunk_index: 0, messages: vec![], summary: None, created_at_ms: 1_000, curated: false, explicit_signal: None, summary_vec: None };
                let mut chunks = w.open_table(CHUNKS_TABLE).unwrap();
                chunks.insert(id, serde_json::to_vec(&chunk).unwrap().as_slice()).unwrap();
                w.open_table(EVIDENCE_TABLE).unwrap().insert(id, b"1".as_slice()).unwrap();
            }
            w.commit().unwrap();
            id
        };
        assert_eq!(list_staged_chunks(&db, 10).unwrap().len(), 1);
        assert_eq!(expire_staged_chunks(&db, 1_000 + 31 * day, 30 * day).unwrap(), 1);
        assert!(list_staged_chunks(&db, 10).unwrap().is_empty());
        let _ = legacy;
        drop(db);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn summaries_and_vectors_are_stored_once_and_old_chunks_still_read() {
        let p = std::env::temp_dir().join(format!("memb_scroll_sum_{}.redb", now_ms()));
        let db = Database::create(&p).unwrap();
        let limits = ScrollLimits { max_messages: 10, retain_tail: 2, min_prompt_chars: 5, dedup_reset_chunks: 3 };
        for i in 0..10 {
            append_message_with(&db, "s", "cc", Role::User, format!("m{i}"), true, &limits).unwrap();
        }
        // A chunk written by an older build has none of the new fields.
        {
            let w = db.begin_write().unwrap();
            {
                let mut t = w.open_table(CHUNKS_TABLE).unwrap();
                let old = r#"{"chunk_id":"chk_old_0000","session_id":"old","chunk_index":0,"messages":[],"summary":null,"created_at_ms":5,"curated":false}"#;
                t.insert("chk_old_0000", old.as_bytes()).unwrap();
            }
            w.commit().unwrap();
        }
        assert_eq!(list_unsummarized_chunks(&db, 10).unwrap().len(), 2);
        assert!(list_unembedded_chunks(&db, 10).unwrap().is_empty());
        assert!(set_chunk_summary(&db, "chk_s_0000", "- a bullet", true).unwrap());
        assert!(!set_chunk_summary(&db, "chk_s_0000", "- another", false).unwrap(), "a summary is written once");
        let unsummarized = list_unsummarized_chunks(&db, 10).unwrap();
        assert_eq!(unsummarized.len(), 1);
        assert_eq!(unsummarized[0].chunk_id, "chk_old_0000");
        assert_eq!(list_unembedded_chunks(&db, 10).unwrap().len(), 1);
        let v = ChunkVec { model: "m1".into(), scale: 0.5, data: vec![1, -2, 3] };
        assert!(!set_chunk_vector(&db, "chk_old_0000", v.clone()).unwrap(), "no vector without a summary");
        assert!(set_chunk_vector(&db, "chk_s_0000", v.clone()).unwrap());
        assert!(!set_chunk_vector(&db, "chk_s_0000", ChunkVec { model: "m2".into(), ..v.clone() }).unwrap());
        assert!(list_unembedded_chunks(&db, 10).unwrap().is_empty());
        assert!(set_chunk_summary(&db, "chk_missing_0000", "x", false).is_err());
        let r = db.begin_read().unwrap();
        let t = r.open_table(CHUNKS_TABLE).unwrap();
        let c: ScrollChunk = serde_json::from_slice(t.get("chk_s_0000").unwrap().unwrap().value()).unwrap();
        assert_eq!((c.summary.as_deref(), c.explicit_signal, c.summary_vec), (Some("- a bullet"), Some(true), Some(v)));
        drop(t);
        drop(r);
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
