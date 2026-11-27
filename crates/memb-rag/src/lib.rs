use memb_core::Note;
use memb_embed::{reciprocal_rank_fusion, QuantizedVec};
use memb_store::Store;

/// Embeddings are opt-in. Set MEMB_EMBED_PORT for a loopback server and
/// MEMB_EMBED_MODEL_ID to a stable model/version identifier to enable caches.
pub fn search(store: &Store, query: &str) -> Result<Vec<Note>, String> {
    memb_core::validate_query(query).map_err(|e| e.to_string())?;
    // `embeddings.enabled = false` in config.toml (or MEMB_EMBED_DISABLE=1) forces lexical search.
    if std::env::var("MEMB_EMBED_DISABLE").ok().as_deref() == Some("1") {
        return hybrid_search(store, query, None, &[]);
    }
    if let Some(directory) = std::env::var_os("MEMB_GEMMA_MODEL_DIR").filter(|p| !p.is_empty()) {
        return search_with_gemma(store, query, std::path::PathBuf::from(directory));
    }
    let port = std::env::var("MEMB_EMBED_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0);
    let model = std::env::var("MEMB_EMBED_MODEL_ID")
        .ok()
        .filter(|s| !s.trim().is_empty() && s.len() <= 512);
    if let Some(port) = port {
        return search_with_server(store, query, port, model.as_deref());
    }
    // Cloud embeddings only when the user configured Snowflake and nothing local was chosen.
    if let Ok(Some(client)) = snowflake_client() {
        return search_with_snowflake(store, query, &client);
    }
    // Supervision is deliberately disabled: reserving a port and then spawning
    // a child cannot establish that the responding listener belongs to it.
    // Never probe or send note content to an unauthenticated child endpoint.
    hybrid_search(store, query, None, &[])
}

type ModelMap = std::sync::Mutex<
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<memb_embed::gemma::GemmaEmbedder>>,
>;

/// How this process reaches the embedding model. Set once at start by `memb` itself; a
/// library user or a test that never sets it loads the model in-process.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// The service's pipe name (see `memb_embed::service::pipe_name`).
    pub name: String,
    /// The executable that is started as `<exe> embed-service` when no service is running.
    pub exe: std::path::PathBuf,
    /// `Wait`: start it and wait for the answer (MCP server, CLI). `Kick`: start it for next
    /// time and answer lexically now (hooks, which must not wait for a model to load).
    pub start: StartMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    Wait,
    Kick,
}

static SERVICE: std::sync::OnceLock<ServiceConfig> = std::sync::OnceLock::new();

/// Set by a `Kick` (hook) process after its first failed or timed-out service request.
#[cfg(windows)]
static SERVICE_GAVE_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn configure_service(config: ServiceConfig) {
    let _ = SERVICE.set(config);
}

/// Starts `<exe> embed-service` detached from this process. It first asks to leave this
/// process's job object (a host-started memb would otherwise take the shared service down
/// with it when it exits); if that is refused it starts the service the ordinary way.
#[cfg(windows)]
pub fn start_service(exe: &std::path::Path) -> std::io::Result<()> {
    // No inherited handles: otherwise a caller that captures this process's output (a host,
    // a shell, a hook runner) would wait for the service to exit before seeing end-of-output.
    memb_win::spawn::spawn_detached(exe, &["embed-service"], true)
        .or_else(|_| memb_win::spawn::spawn_detached(exe, &["embed-service"], false))
}

/// How long one request to a running service may take. A hook must answer within its host's
/// timeout, and a service whose model was unloaded while idle needs 10 s or more to load it
/// again, so a hook gives up quickly (lexical answer now; the request has already made the
/// service start loading, so the next one is fast).
pub fn call_deadline(mode: StartMode) -> std::time::Duration {
    match mode {
        StartMode::Kick => std::time::Duration::from_secs(2),
        StartMode::Wait => std::time::Duration::from_secs(90),
    }
}

/// Embeds through the shared service. `Err` means "no vector now": the callers fall back
/// to lexical search, they never load the model themselves.
#[cfg(windows)]
fn service_embed(config: &ServiceConfig, text: &str, is_query: bool) -> Result<(String, Vec<f32>), String> {
    use memb_embed::service::{call, CallError, Request};
    use std::time::{Duration, Instant};
    let request = Request::Embed { text: text.to_owned(), query: is_query };
    let answer = |r: memb_embed::service::Response| {
        if r.ok {
            match (r.tag, r.vector) {
                (Some(tag), Some(vector)) => Ok((tag, vector)),
                _ => Err("the embedding service answered without a vector".to_string()),
            }
        } else {
            Err(r.error.unwrap_or_else(|| "the embedding service failed".into()))
        }
    };
    // A hook embeds several texts; once one request has failed or timed out the rest would each
    // wait the full deadline again, so they are answered lexically at once.
    if config.start == StartMode::Kick && SERVICE_GAVE_UP.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("the embedding service is not answering in time; lexical search for now".into());
    }
    match call(&config.name, &request, call_deadline(config.start)) {
        Ok(r) => answer(r),
        Err(CallError::Failed(reason)) => {
            if config.start == StartMode::Kick {
                SERVICE_GAVE_UP.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(reason)
        }
        Err(CallError::NotRunning) => {
            start_service(&config.exe).map_err(|e| format!("cannot start the embedding service: {e}"))?;
            if config.start == StartMode::Kick {
                return Err("the embedding service was just started; lexical search for now".into());
            }
            // The pipe appears within a moment; the first embedding then waits for the model to load.
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
                match call(&config.name, &request, Duration::from_secs(120)) {
                    Err(CallError::NotRunning) => continue,
                    Ok(r) => return answer(r),
                    Err(CallError::Failed(reason)) => return Err(reason),
                }
            }
            Err("the embedding service did not come up in time".into())
        }
    }
}

/// One embedding by the local model: through the service when this process was configured
/// for it (and is not the service itself), otherwise in this process (the service, tests,
/// `MEMB_EMBED_SERVICE=0`).
fn gemma_embed(directory: &std::path::Path, text: &str, is_query: bool) -> Result<(String, Vec<f32>), String> {
    let in_process = std::env::var("MEMB_IN_SERVICE").ok().as_deref() == Some("1")
        || std::env::var("MEMB_EMBED_SERVICE").ok().as_deref() == Some("0");
    #[cfg(windows)]
    if !in_process {
        if let Some(config) = SERVICE.get() {
            return service_embed(config, text, is_query);
        }
    }
    let _ = in_process;
    gemma_model(directory.to_path_buf()).ok_or("embedding model unavailable")?.embed_tagged(text, is_query)
}

fn model_map() -> &'static ModelMap {
    static MODELS: std::sync::OnceLock<ModelMap> = std::sync::OnceLock::new();
    MODELS.get_or_init(Default::default)
}

fn query_cache() -> &'static std::sync::Mutex<memb_embed::QueryLru> {
    static QUERIES: std::sync::OnceLock<std::sync::Mutex<memb_embed::QueryLru>> = std::sync::OnceLock::new();
    QUERIES.get_or_init(|| std::sync::Mutex::new(memb_embed::QueryLru::new(memb_embed::QUERY_LRU_CAP)))
}

fn clear_query_cache() {
    if let Ok(mut cache) = query_cache().lock() {
        cache.clear();
    }
}

/// Drops every in-process embedding model that has been unused for `idle` (the memory is
/// returned; the next search loads it again). Returns how many were dropped.
pub fn unload_idle_models(idle: std::time::Duration) -> usize {
    let Ok(models) = model_map().lock() else { return 0 };
    models.values().filter(|m| m.unload_if_idle(idle)).count()
}

/// Memory pressure: drops every in-process embedding model and the query cache now.
/// Returns how many models were dropped.
pub fn trim_caches() -> usize {
    let dropped = model_map()
        .lock()
        .map(|models| models.values().filter(|m| m.unload()).count())
        .unwrap_or(0);
    clear_query_cache();
    if let Ok(mut hot) = hot_cache().lock() {
        hot.clear();
    }
    dropped
}

/// One embedding model per directory, loaded on first use and unloaded when idle.
fn gemma_model(
    directory: std::path::PathBuf,
) -> Option<std::sync::Arc<memb_embed::gemma::GemmaEmbedder>> {
    use std::sync::Arc;
    let mut models = model_map().lock().ok()?;
    Some(
        models
            .entry(directory.clone())
            .or_insert_with(|| Arc::new(memb_embed::gemma::GemmaEmbedder::new(directory)))
            .clone(),
    )
}

/// Embeds one text with the configured backend, the same selection `search` uses
/// (`MEMB_EMBED_DISABLE`, then `MEMB_GEMMA_MODEL_DIR`, then `MEMB_EMBED_PORT`).
/// Returns the vector with a tag naming the exact model and truncation, so vectors from
/// different models are never compared. `Ok(None)` means no usable backend is
/// configured (lexical-only), which is not an error. The loopback-server path needs
/// `MEMB_EMBED_MODEL_ID`, because without a stable identity the vector cannot be tagged.
pub fn embed_text(text: &str, is_query: bool) -> Result<Option<(String, QuantizedVec)>, String> {
    if std::env::var("MEMB_EMBED_DISABLE").ok().as_deref() == Some("1") {
        return Ok(None);
    }
    let mrl = memb_embed::mrl_dim_from_env()?;
    if let Some(directory) = std::env::var_os("MEMB_GEMMA_MODEL_DIR").filter(|p| !p.is_empty()) {
        let (tag, v) = gemma_embed(std::path::Path::new(&directory), text, is_query)?;
        return Ok(Some((
            memb_embed::mrl_tag(&tag, mrl),
            QuantizedVec::from_f32(&memb_embed::apply_mrl(v, mrl)?),
        )));
    }
    let port = std::env::var("MEMB_EMBED_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0);
    let model = std::env::var("MEMB_EMBED_MODEL_ID")
        .ok()
        .filter(|s| !s.trim().is_empty() && s.len() <= 512);
    match (port, model) {
        (Some(port), Some(model)) => {
            // The same prompt formats as `search`: the query or document prefix of EmbeddingGemma.
            let sent = if is_query {
                memb_embed::server_query_text(text)
            } else {
                format!("{}{text}", memb_embed::gemma::DOC_PREFIX)
            };
            let v = memb_embed::local_embedding(port, &sent)?;
            Ok(Some((server_tag(&model, mrl), v)))
        }
        _ => match snowflake_client()? {
            Some(client) => {
                let mrl = memb_embed::mrl_dim_from_env()?;
                let sent = if is_query { format!("{}{text}", memb_embed::snowflake::query_prefix(client.model())) } else { text.to_owned() };
                let v = embed_snowflake(&client, &[sent], mrl)?.pop().ok_or("Snowflake returned no vector")?;
                Ok(Some((memb_embed::mrl_tag(&memb_embed::snowflake::identity(client.model()), mrl), v)))
            }
            None => Ok(None),
        },
    }
}

type SnowflakeSlot = std::sync::Mutex<Option<(String, std::sync::Arc<memb_embed::snowflake::Client>)>>;

/// The Snowflake client for this process (the key is read once), or `None` when Snowflake is
/// not configured. A broken configuration is an error.
fn snowflake_client() -> Result<Option<std::sync::Arc<memb_embed::snowflake::Client>>, String> {
    static CLIENT: std::sync::OnceLock<SnowflakeSlot> = std::sync::OnceLock::new();
    let Some(cfg) = memb_embed::snowflake::config_from_env()? else { return Ok(None) };
    let id = format!("{cfg:?}");
    let mut guard = CLIENT.get_or_init(Default::default).lock().map_err(|_| "snowflake client lock poisoned".to_string())?;
    if let Some((key, client)) = guard.as_ref() {
        if *key == id {
            return Ok(Some(client.clone()));
        }
    }
    let client = std::sync::Arc::new(memb_embed::snowflake::Client::new(cfg)?);
    *guard = Some((id, client.clone()));
    Ok(Some(client))
}

fn embed_snowflake(
    client: &memb_embed::snowflake::Client,
    texts: &[String],
    mrl: Option<usize>,
) -> Result<Vec<QuantizedVec>, String> {
    client
        .embed_batch(texts)?
        .into_iter()
        .map(|v| Ok(QuantizedVec::from_f32(&memb_embed::apply_mrl(v, mrl)?)))
        .collect()
}

fn search_with_snowflake(
    store: &Store,
    query: &str,
    client: &std::sync::Arc<memb_embed::snowflake::Client>,
) -> Result<Vec<Note>, String> {
    let Ok(mrl) = memb_embed::mrl_dim_from_env() else {
        return hybrid_search(store, query, None, &[]);
    };
    let tag = memb_embed::mrl_tag(&memb_embed::snowflake::identity(client.model()), mrl);
    let cache = query_cache();
    let key = format!("sf:{tag}:{}:{query}", query.len());
    let cached = cache.lock().ok().and_then(|mut c| c.get(&key).cloned());
    let q = match cached {
        Some(v) => v,
        None => {
            let sent = format!("{}{query}", memb_embed::snowflake::query_prefix(client.model()));
            match embed_snowflake(client, &[sent], mrl).ok().and_then(|mut v| v.pop()) {
                Some(v) => v,
                None => return hybrid_search(store, query, None, &[]),
            }
        }
    };
    if let Ok(mut c) = cache.lock() {
        c.insert(key, q.clone());
    }
    // Notes that are not cached yet go out in batches (one statement per 16), not one by one.
    let (client, width) = (client.clone(), q.data.len());
    let embed: NoteEmbedder = std::sync::Arc::new(move |notes: &[&Note]| {
        let texts: Vec<String> = notes.iter().map(|n| memb_embed::snowflake::document_text(&n.title, &n.desc)).collect();
        embed_snowflake(&client, &texts, mrl).ok().filter(|v| v.iter().all(|x| x.data.len() == width))
    });
    search_with_index(store, query, &q, Backend::Snowflake, hot_set_nowait(store, &tag, embed)?)
}

/// What a note is embedded as by the active backend, with the tag of the vector.
/// `Ok(None)` when no embedding backend is configured.
pub fn embed_note(title: &str, desc: &str) -> Result<Option<(String, QuantizedVec)>, String> {
    let local = std::env::var_os("MEMB_GEMMA_MODEL_DIR").is_some_and(|p| !p.is_empty())
        || std::env::var("MEMB_EMBED_PORT").ok().and_then(|p| p.parse::<u16>().ok()).is_some_and(|p| p != 0);
    if !local && std::env::var("MEMB_EMBED_DISABLE").ok().as_deref() != Some("1") {
        if snowflake_client()?.is_some() {
            return embed_text(&memb_embed::snowflake::document_text(title, desc), false);
        }
    }
    embed_text(&memb_embed::document_text(title, desc), false)
}

/// Vectors of every active knowledge and claim note, embedded with the configured
/// backend, for linking chunks to notes. Uses and fills the same durable document cache
/// as search, so unchanged notes are never embedded twice. Returns the model tag the
/// vectors carry (chunk summary vectors with the same tag are comparable), or `Ok(None)`
/// when no embedding backend is configured. A note that fails to embed is skipped.
pub fn note_vectors(store: &Store) -> Result<Option<(String, Vec<(u64, QuantizedVec)>)>, String> {
    // The tag depends on the model and truncation, so ask the backend once with a tiny text.
    let Some((tag, probe)) = embed_text("memb", false)? else {
        return Ok(None);
    };
    let notes = store.load_all_notes()?;
    let wanted: Vec<&Note> = notes
        .values()
        .filter(|n| {
            n.status == memb_core::NoteStatus::Active
                && matches!(n.kind, memb_core::Kind::Knowledge | memb_core::Kind::Claim)
        })
        .collect();
    let keys: Vec<[u8; 32]> = wanted
        .iter()
        .map(|n| memb_embed::document_key(&tag, &n.title, &n.desc))
        .collect();
    let cached = store
        .with_db(|db| memb_embed::get_document_vectors(db, &keys))
        .unwrap_or_default();
    let mut fresh = Vec::new();
    let mut out = Vec::new();
    for (note, key) in wanted.iter().zip(&keys) {
        if let Some(v) = cached.get(key).filter(|v| v.data.len() == probe.data.len()) {
            out.push((note.id, v.clone()));
            continue;
        }
        match embed_note(&note.title, &note.desc) {
            Ok(Some((t, v))) if t == tag && v.data.len() == probe.data.len() => {
                fresh.push((*key, v.clone()));
                out.push((note.id, v));
            }
            _ => continue,
        }
    }
    if !fresh.is_empty() {
        let _ = store.with_db(|db| memb_embed::put_document_vectors(db, &fresh));
    }
    Ok(Some((tag, out)))
}

type HotVectors = std::sync::Arc<Vec<(u64, QuantizedVec)>>;
type HotCache = std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, String), (u64, HotVectors)>>;

fn hot_cache() -> &'static HotCache {
    static HOT: std::sync::OnceLock<HotCache> = std::sync::OnceLock::new();
    HOT.get_or_init(Default::default)
}

/// The in-process copy of a hot set, if it was built at exactly `version`.
pub fn hot_cache_lookup(root: &std::path::Path, tag: &str, version: u64) -> Option<HotVectors> {
    let cache = hot_cache().lock().ok()?;
    cache
        .get(&(root.to_path_buf(), tag.to_owned()))
        .filter(|(v, _)| *v == version)
        .map(|(_, h)| h.clone())
}

fn hot_cache_store(root: &std::path::Path, tag: &str, version: u64, hot: HotVectors) {
    if let Ok(mut cache) = hot_cache().lock() {
        cache.insert((root.to_path_buf(), tag.to_owned()), (version, hot));
    }
}

/// How `hot_set` found its answer (for tests and diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotSource {
    Memory,
    Disk,
    Rebuilt { embedded: usize },
}

/// The working set for one model tag: the i8 vector of every active note, valid for one
/// `project_kb_version`. Looks in this process first, then in the persisted row; only when
/// the knowledge base changed since either was built are the notes read again, and then
/// only notes whose content hash is not in the document cache are embedded. `embed_doc`
/// returns `None` when a note cannot be embedded, which makes the whole call `Ok(None)`
/// (the caller then falls back to lexical search rather than ranking a partial set).
pub fn hot_set(
    store: &Store,
    tag: &str,
    mut embed_doc: impl FnMut(&Note) -> Option<QuantizedVec>,
) -> Result<Option<(HotVectors, HotSource)>, String> {
    hot_set_batch(store, tag, |notes| notes.iter().map(|n| embed_doc(n)).collect::<Option<Vec<_>>>())
}

/// Like `hot_set`, but the notes that are not cached are handed over in one list, so a
/// backend with a per-request cost (Snowflake) can embed them in batches. `embed_docs` must
/// return one vector per note, in order, or `None`.
pub fn hot_set_batch(
    store: &Store,
    tag: &str,
    mut embed_docs: impl FnMut(&[&Note]) -> Option<Vec<QuantizedVec>>,
) -> Result<Option<(HotVectors, HotSource)>, String> {
    let version = store.kb_version()?;
    if let Some(hit) = hot_cache_lookup(store.root(), tag, version) {
        return Ok(Some((hit, HotSource::Memory)));
    }
    if let Some((built, entries)) = store.with_db(|db| memb_embed::read_hot_set(db, tag))? {
        if built == version {
            let hot = std::sync::Arc::new(entries);
            hot_cache_store(store.root(), tag, version, hot.clone());
            return Ok(Some((hot, HotSource::Disk)));
        }
    }
    let notes = store.load_all_notes()?;
    let active: Vec<&Note> = notes
        .values()
        .filter(|n| n.status == memb_core::NoteStatus::Active)
        .collect();
    let keys: Vec<[u8; 32]> = active
        .iter()
        .map(|n| memb_embed::document_key(tag, &n.title, &n.desc))
        .collect();
    let mut all = store
        .with_db(|db| memb_embed::get_document_vectors(db, &keys))
        .unwrap_or_default();
    let missing: Vec<(&Note, [u8; 32])> = active
        .iter()
        .zip(&keys)
        .filter(|(_, k)| !all.contains_key(*k))
        .map(|(n, k)| (*n, *k))
        .collect();
    // Notes are embedded in chunks and every chunk is saved at once, so a build that is
    // interrupted (a failed embedding, a killed process) resumes from the notes already done.
    let mut embedded = 0usize;
    for chunk in missing.chunks(HOT_CHUNK) {
        let notes: Vec<&Note> = chunk.iter().map(|(n, _)| *n).collect();
        let vectors = match embed_docs(&notes) {
            Some(v) if v.len() == chunk.len() => v,
            _ => return Ok(None),
        };
        let done: Vec<([u8; 32], QuantizedVec)> = chunk.iter().map(|(_, k)| *k).zip(vectors).collect();
        let _ = store.with_db(|db| memb_embed::put_document_vectors(db, &done));
        embedded += done.len();
        all.extend(done);
    }
    let mut entries: Vec<(u64, QuantizedVec)> = Vec::new();
    for (note, key) in active.iter().zip(&keys) {
        let vector = all.get(key).ok_or("batch embedding ran out of vectors")?.clone();
        if entries.first().is_some_and(|(_, f)| f.data.len() != vector.data.len()) {
            return Ok(None);
        }
        entries.push((note.id, vector));
    }
    let _ = store.with_db(|db| memb_embed::write_hot_set(db, tag, version, &entries));
    let hot = std::sync::Arc::new(entries);
    hot_cache_store(store.root(), tag, version, hot.clone());
    Ok(Some((hot, HotSource::Rebuilt { embedded })))
}

/// How many notes are embedded (and saved) at a time while a hot set is built.
pub const HOT_CHUNK: usize = 16;

static BACKGROUND_BUILD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Long-lived processes (the MCP server, the daemon) call this so that a search that finds
/// the hot set missing starts building it in the background. Short-lived ones (hooks, the
/// CLI) leave it off: they answer from what is cached and never wait for, or start, a build.
pub fn set_background_build(on: bool) {
    BACKGROUND_BUILD.store(on, std::sync::atomic::Ordering::Relaxed);
}

type BuildGuard = std::sync::Mutex<std::collections::HashSet<(std::path::PathBuf, String)>>;

fn builds_in_progress() -> &'static BuildGuard {
    static BUILDS: std::sync::OnceLock<BuildGuard> = std::sync::OnceLock::new();
    BUILDS.get_or_init(Default::default)
}

/// Blocks until no background hot-set build is running in this process (or `timeout`
/// passes); true if none is. For benchmarks and tools that need the complete index.
pub fn wait_for_background_builds(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if builds_in_progress().lock().map(|b| b.is_empty()).unwrap_or(true) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether a background build of this set is running in this process.
pub fn build_in_progress(root: &std::path::Path, tag: &str) -> bool {
    builds_in_progress().lock().map(|b| b.contains(&(root.to_path_buf(), tag.to_owned()))).unwrap_or(false)
}

/// What `hot_set_nowait` could give a search right now.
pub enum HotAnswer {
    /// The complete set for the current knowledge base.
    Complete(HotVectors, HotSource),
    /// The vectors that exist already (notes not embedded yet are missing): rank with these
    /// plus lexical search. `building` says whether a background build is running.
    Partial { vectors: HotVectors, embedded: usize, total: usize, building: bool },
}

/// Like `hot_set_batch`, but it never embeds on the caller's thread. A complete set (memory,
/// disk, or every vector already cached) is returned as it is; otherwise the cached vectors
/// are returned at once and, when `set_background_build(true)`, the missing ones are embedded
/// in a background thread that saves its progress chunk by chunk.
pub fn hot_set_nowait(
    store: &Store,
    tag: &str,
    embed_docs: std::sync::Arc<dyn Fn(&[&Note]) -> Option<Vec<QuantizedVec>> + Send + Sync>,
) -> Result<HotAnswer, String> {
    let version = store.kb_version()?;
    if let Some(hit) = hot_cache_lookup(store.root(), tag, version) {
        return Ok(HotAnswer::Complete(hit, HotSource::Memory));
    }
    if let Some((built, entries)) = store.with_db(|db| memb_embed::read_hot_set(db, tag))? {
        if built == version {
            let hot = std::sync::Arc::new(entries);
            hot_cache_store(store.root(), tag, version, hot.clone());
            return Ok(HotAnswer::Complete(hot, HotSource::Disk));
        }
    }
    let notes = store.load_all_notes()?;
    let active: Vec<&Note> = notes.values().filter(|n| n.status == memb_core::NoteStatus::Active).collect();
    let keys: Vec<[u8; 32]> = active.iter().map(|n| memb_embed::document_key(tag, &n.title, &n.desc)).collect();
    let cached = store.with_db(|db| memb_embed::get_document_vectors(db, &keys)).unwrap_or_default();
    if cached.len() >= active.len() && keys.iter().all(|k| cached.contains_key(k)) {
        // Nothing to embed (a note was retracted, or reworded to text that is already cached):
        // assembling the set embeds nothing, so the embedder is never called.
        return match hot_set_batch(store, tag, |_| None)? {
            Some((hot, source)) => Ok(HotAnswer::Complete(hot, source)),
            None => Err("the hot set could not be assembled from cached vectors".into()),
        };
    }
    let entries: Vec<(u64, QuantizedVec)> = active.iter().zip(&keys).filter_map(|(n, k)| cached.get(k).map(|v| (n.id, v.clone()))).collect();
    let (embedded, total) = (entries.len(), active.len());
    let mut building = build_in_progress(store.root(), tag);
    if !building && BACKGROUND_BUILD.load(std::sync::atomic::Ordering::Relaxed) {
        let key = (store.root().to_path_buf(), tag.to_owned());
        let started = builds_in_progress().lock().map(|mut b| b.insert(key.clone())).unwrap_or(false);
        if started {
            building = true;
            let (store, tag) = (store.clone(), tag.to_owned());
            std::thread::spawn(move || {
                struct Done((std::path::PathBuf, String));
                impl Drop for Done {
                    fn drop(&mut self) {
                        if let Ok(mut b) = builds_in_progress().lock() {
                            b.remove(&self.0);
                        }
                    }
                }
                let _done = Done(key);
                let _ = hot_set_batch(&store, &tag, |notes| embed_docs(notes));
            });
        }
    }
    Ok(HotAnswer::Partial { vectors: std::sync::Arc::new(entries), embedded, total, building })
}

fn search_with_gemma(
    store: &Store,
    query: &str,
    directory: std::path::PathBuf,
) -> Result<Vec<Note>, String> {
    // An unsupported MEMB_EMBED_MRL_DIM or a failed embedding degrades to lexical search.
    let Ok(mrl) = memb_embed::mrl_dim_from_env() else {
        return hybrid_search(store, query, None, &[]);
    };
    let (tag, q) = match gemma_embed(&directory, query, true).and_then(|(tag, v)| {
        Ok((memb_embed::mrl_tag(&tag, mrl), memb_embed::apply_mrl(v, mrl)?))
    }) {
        Ok((tag, v)) => (tag, QuantizedVec::from_f32(&v)),
        Err(_) => return hybrid_search(store, query, None, &[]),
    };
    let embed: NoteEmbedder = {
        let (tag, width) = (tag.clone(), q.data.len());
        std::sync::Arc::new(move |notes: &[&Note]| {
            notes
                .iter()
                .map(|note| {
                    let (doc_tag, v) = gemma_embed(&directory, &memb_embed::document_text(&note.title, &note.desc), false).ok()?;
                    let v = memb_embed::apply_mrl(v, mrl).ok()?;
                    (memb_embed::mrl_tag(&doc_tag, mrl) == tag && v.len() == width).then(|| QuantizedVec::from_f32(&v))
                })
                .collect()
        })
    };
    search_with_index(store, query, &q, Backend::Gemma, hot_set_nowait(store, &tag, embed)?)
}

/// Embeds a list of notes; `None` when any of them cannot be embedded.
type NoteEmbedder = std::sync::Arc<dyn Fn(&[&Note]) -> Option<Vec<QuantizedVec>> + Send + Sync>;

/// Ranks with whatever vectors exist: the complete set, or the cached part of one that is
/// still being built (the rest is found by lexical search until the build finishes).
fn search_with_index(store: &Store, query: &str, q: &QuantizedVec, backend: Backend, hot: HotAnswer) -> Result<Vec<Note>, String> {
    match hot {
        HotAnswer::Complete(index, _) | HotAnswer::Partial { vectors: index, .. } => fuse(store, query, Some(q), &index, true, backend),
    }
}

/// The tag of vectors made by a loopback server: the model identity, the prompt format
/// (so vectors made without the EmbeddingGemma prefixes are never reused) and the truncation.
pub fn server_tag(model: &str, mrl: Option<usize>) -> String {
    memb_embed::mrl_tag(&format!("{model}|prompt:gemma-v1"), mrl)
}

fn search_with_server(
    store: &Store,
    query: &str,
    port: u16,
    model: Option<&str>,
) -> Result<Vec<Note>, String> {
    let Ok(mrl) = memb_embed::mrl_dim_from_env() else {
        return hybrid_search(store, query, None, &[]);
    };
    // Without a stable model identity a vector cannot be tagged or cached, and embedding
    // every note on every query is not an option: lexical search only.
    let Some(model) = model else {
        return hybrid_search(store, query, None, &[]);
    };
    let tag = server_tag(model, mrl);
    let cache = query_cache();
    let key = format!("{port}:{tag}:{}:{query}", query.len());
    let cached = cache.lock().ok().and_then(|mut c| c.get(&key).cloned());
    let query_vec = match cached.or_else(|| memb_embed::local_embedding(port, &memb_embed::server_query_text(query)).ok()) {
        Some(v) => v,
        None => return hybrid_search(store, query, None, &[]),
    };
    if let Ok(mut c) = cache.lock() {
        c.insert(key, query_vec.clone());
    }
    let width = query_vec.data.len();
    let embed: NoteEmbedder = std::sync::Arc::new(move |notes: &[&Note]| {
        notes
            .iter()
            .map(|note| {
                memb_embed::local_embedding(port, &memb_embed::server_document_text(&note.title, &note.desc))
                    .ok()
                    .filter(|v| v.data.len() == width)
            })
            .collect()
    });
    search_with_index(store, query, &query_vec, Backend::Loopback, hot_set_nowait(store, &tag, embed)?)
}

/// Hybrid retrieval combining lexical ranking + quantized vector scan + RRF (k=60) + 1-hop graph expansion.
/// Gracefully degrades to pure lexical search when `query_vec` is None (e.g. llama-server unavailable).
/// `hot_index` may hold notes that are no longer active; they are filtered out here.
pub fn hybrid_search(
    store: &Store,
    query: &str,
    query_vec: Option<&QuantizedVec>,
    hot_index: &[(u64, QuantizedVec)],
) -> Result<Vec<Note>, String> {
    fuse(store, query, query_vec, hot_index, false, Backend::Other)
}

/// Notes a search starts from (before one-hop expansion) when `MEMB_RETRIEVAL_TOP_K` is unset.
pub const DEFAULT_TOP_K: usize = 5;
/// The embedding backend a vector score came from: cosine ranges differ per model, so the
/// default score floor is chosen per backend (T032, T045).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The local EmbeddingGemma model (in this process or through the shared service).
    Gemma,
    /// Snowflake `AI_EMBED`.
    Snowflake,
    /// A loopback llama-server.
    Loopback,
    /// Vectors handed in by a library caller: nothing is known about their scores.
    Other,
}

/// A vector match scoring at or below this cosine is dropped for a backend that was
/// measured with the `injection_floor_sweep` benches. EmbeddingGemma q4 at 256 dimensions:
/// unrelated notes score about 0.6 there, so the older 0.25 filtered nothing
/// (evidence/t032-injection-floor.md).
pub const GEMMA_SCORE_FLOOR: f32 = 0.60;
/// Snowflake `AI_EMBED` (snowflake-arctic-embed-m-v1.5, 768-wide i8) scores much lower than
/// the local model: right notes 0.239 to 0.577, the best off-topic match 0.260, so a floor of
/// 0.60 would drop every vector hit (hybrid recall@5 falls from 1.00 to 0.63). 0.25 keeps 29 of
/// 30 right notes and lets 1 of 12 off-topic questions through (evidence/t045-score-floor-per-backend.md).
pub const SNOWFLAKE_SCORE_FLOOR: f32 = 0.25;
/// A loopback llama-server (EmbeddingGemma 300M Q8_0, `--embedding --pooling mean`): right
/// notes 0.206 to 0.657 and the best off-topic match 0.166 at 768 wide, right notes 0.313 to 0.690
/// and the best off-topic match 0.296 at 256 wide; 0.25 keeps 29 and 30 of 30 right notes and gives
/// hybrid recall@5 1.00 and 0.97 (evidence/t045-score-floor-per-backend.md).
pub const LOOPBACK_SCORE_FLOOR: f32 = 0.25;
/// The floor for a backend whose scores were not measured: permissive on purpose, so an
/// unmeasured backend can never lose all its vector hits to a floor tuned for another model.
pub const UNMEASURED_SCORE_FLOOR: f32 = 0.25;

/// The score floor used for `backend` when `[retrieval] score_floor` / `MEMB_RETRIEVAL_SCORE_FLOOR`
/// does not set one.
pub fn default_score_floor(backend: Backend) -> f32 {
    match backend {
        Backend::Gemma => GEMMA_SCORE_FLOOR,
        Backend::Snowflake => SNOWFLAKE_SCORE_FLOOR,
        Backend::Loopback => LOOPBACK_SCORE_FLOOR,
        Backend::Other => UNMEASURED_SCORE_FLOOR,
    }
}

/// `(top_k, score floor)` for a search that used `backend`: from the environment (`memb`
/// fills it from `[retrieval]` in config.toml; an unset or invalid value means the default,
/// and the default floor depends on the backend).
pub fn retrieval_params(backend: Backend) -> (usize, f32) {
    parse_params(
        std::env::var("MEMB_RETRIEVAL_TOP_K").ok().as_deref(),
        std::env::var("MEMB_RETRIEVAL_SCORE_FLOOR").ok().as_deref(),
        backend,
    )
}

fn parse_params(top_k: Option<&str>, floor: Option<&str>, backend: Backend) -> (usize, f32) {
    let top_k = top_k
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|k| (1..=20).contains(k))
        .unwrap_or(DEFAULT_TOP_K);
    let floor = floor
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|f| (-1.0..=1.0).contains(f))
        .unwrap_or_else(|| default_score_floor(backend));
    (top_k, floor)
}

/// `index_is_active`: every id in `hot_index` is an active note (a hot set is built from
/// active notes at the current version), so no note needs to be read to check it.
fn fuse(
    store: &Store,
    query: &str,
    query_vec: Option<&QuantizedVec>,
    hot_index: &[(u64, QuantizedVec)],
    index_is_active: bool,
    backend: Backend,
) -> Result<Vec<Note>, String> {
    let (top_k, floor) = retrieval_params(backend);
    fuse_with(store, query, query_vec, hot_index, index_is_active, top_k, floor)
}

fn fuse_with(
    store: &Store,
    query: &str,
    query_vec: Option<&QuantizedVec>,
    hot_index: &[(u64, QuantizedVec)],
    index_is_active: bool,
    top_k: usize,
    floor: f32,
) -> Result<Vec<Note>, String> {
    let lexical_ids = store.search_lexical_ids(query, 10)?;
    let fused_seeds = if let Some(qv) = query_vec {
        let all = if index_is_active { None } else { Some(store.load_all_notes()?) };
        let mut vec_scored: Vec<(f32, u64)> = hot_index
            .iter()
            .filter(|(id, _)| {
                all.as_ref().is_none_or(|all| {
                    all.get(id).is_some_and(|n| n.status == memb_core::NoteStatus::Active)
                })
            })
            .map(|(id, nv)| (qv.cosine(nv), *id))
            .filter(|(s, _)| *s > floor)
            .collect();
        vec_scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let vector_ids: Vec<u64> = vec_scored.into_iter().take(10).map(|(_, id)| id).collect();
        // A note retracted after the hot set was built must not take a seed slot.
        let vector_ids = if index_is_active { store.filter_active(&vector_ids)? } else { vector_ids };
        reciprocal_rank_fusion(&lexical_ids, &vector_ids, top_k)
    } else {
        lexical_ids.into_iter().take(top_k).collect()
    };
    store.expand_one_hop(&fused_seeds, 8.max(top_k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use memb_core::{Draft, Kind};
    use std::collections::BTreeMap;

    #[test]
    fn a_hook_gives_up_on_a_slow_service_far_sooner_than_the_hook_timeout() {
        let hook = call_deadline(StartMode::Kick);
        assert!(hook <= std::time::Duration::from_secs(3), "well inside the 10 s hook timeout");
        assert!(call_deadline(StartMode::Wait) >= std::time::Duration::from_secs(60), "a search waits for the model to load");
    }

    /// A real pipe service whose model "takes 3 s to load": a hook (kick mode) must give up
    /// after its 2 s deadline and then skip the service at once, while a search (wait mode)
    /// gets the answer.
    #[cfg(windows)]
    #[test]
    fn a_slow_service_makes_a_hook_give_up_after_the_deadline_and_a_search_wait_for_the_answer() {
        use memb_embed::service::{call, serve, Request, Response};
        use std::time::{Duration, Instant};
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("memb-rag-slow-{}-{nanos}", std::process::id());
        let slow = |request: Request| match request {
            Request::Embed { query, .. } => {
                std::thread::sleep(Duration::from_secs(3));
                Response { ok: true, tag: Some(if query { "slow|query" } else { "slow|doc" }.into()), vector: Some(vec![1.0, 2.0]), ..Response::default() }
            }
            _ => Response { ok: true, version: Some("test".into()), ..Response::default() },
        };
        let server = {
            let name = name.clone();
            std::thread::spawn(move || serve(&name, std::sync::Arc::new(slow), Duration::from_secs(5)))
        };
        let mut up = false;
        for _ in 0..100 {
            if call(&name, &Request::Ping, Duration::from_secs(2)).is_ok() {
                up = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(up, "the test service came up");
        let config = |start| ServiceConfig { name: name.clone(), exe: std::path::PathBuf::from("not-started-by-this-test"), start };

        SERVICE_GAVE_UP.store(false, std::sync::atomic::Ordering::Relaxed);
        let started = Instant::now();
        let first = service_embed(&config(StartMode::Kick), "hello", true);
        let first_took = started.elapsed();
        assert!(first.is_err(), "a hook does not wait for the slow service: {first:?}");
        assert!(first_took >= Duration::from_millis(1800) && first_took < Duration::from_millis(2900), "it gave up at its 2 s deadline, took {first_took:?}");

        let started = Instant::now();
        let second = service_embed(&config(StartMode::Kick), "world", false);
        assert!(second.is_err());
        assert!(started.elapsed() < Duration::from_millis(300), "after the first failure the rest of the hook skips the service, took {:?}", started.elapsed());
        SERVICE_GAVE_UP.store(false, std::sync::atomic::Ordering::Relaxed);

        let started = Instant::now();
        let waited = service_embed(&config(StartMode::Wait), "hello", true).expect("a search waits for the answer");
        assert_eq!(waited, ("slow|query".to_string(), vec![1.0, 2.0]));
        assert!(started.elapsed() >= Duration::from_millis(2900), "it really waited for the slow handler, took {:?}", started.elapsed());
        drop(server);
    }

    /// The environment is process-wide: serialize the tests that change it.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Waits until no background hot-set build is running in this process.
    fn wait_for_builds() {
        wait_for_builds_within(30);
    }

    fn wait_for_builds_within(seconds: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while builds_in_progress().lock().map(|b| !b.is_empty()).unwrap_or(false) {
            assert!(std::time::Instant::now() < deadline, "a background build did not finish");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A store with a topic and `n` knowledge notes (`n + 1` active notes).
    fn store_with_notes(label: &str, n: usize) -> Store {
        let root = std::env::temp_dir().join(format!(
            "memb-rag-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(&root).unwrap();
        let draft = |title: String, desc: String, kind: Kind, parents: Vec<u64>| Draft { title, desc, kind, parents, properties: BTreeMap::new() };
        let topic = store.remember(draft("topic".into(), "root".into(), Kind::Topic, vec![])).unwrap();
        for i in 0..n {
            store
                .remember(draft(format!("note {i}"), format!("description of note {i} about subject {}", i % 7), Kind::Knowledge, vec![topic.id]))
                .unwrap();
        }
        store
    }

    /// A note embedder that takes `delay` per note and counts the notes it was asked for.
    fn slow_embedder(delay: std::time::Duration, count: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> NoteEmbedder {
        std::sync::Arc::new(move |notes: &[&Note]| {
            let mut out = Vec::new();
            for n in notes {
                std::thread::sleep(delay);
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                out.push(fake_vector(&memb_embed::document_text(&n.title, &n.desc)));
            }
            Some(out)
        })
    }

    #[test]
    fn a_search_never_waits_for_the_vector_index_and_the_background_build_completes_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let store = store_with_notes("nowait", 40);
        let tag = "nowait-model|768";
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        // 41 notes at 150 ms each: a blocking build would take about 6 s, so a search that
        // returns within 2 s (generous: the machine may be busy) cannot have waited for it.
        let embed = slow_embedder(Duration::from_millis(150), count.clone());
        set_background_build(true);

        let started = Instant::now();
        let first = hot_set_nowait(&store, tag, embed.clone()).unwrap();
        let first_took = started.elapsed();
        // Wall-clock limits are unreliable on a busy machine, so the proof that the search did
        // not wait is the state at the moment it returned: the build is still running and most
        // notes are not embedded yet (a search that waited would see all 41 done).
        assert!(count.load(Ordering::SeqCst) < 41, "the build had already finished when the search returned (took {first_took:?})");
        assert!(build_in_progress(store.root(), tag), "the build runs in the background (took {first_took:?})");
        assert!(first_took < Duration::from_secs(5), "the first search took far too long: {first_took:?}");
        match first {
            HotAnswer::Partial { vectors, embedded, total, building } => {
                assert_eq!((vectors.len(), embedded, total), (0, 0, 41));
                assert!(building, "the build runs in the background");
            }
            HotAnswer::Complete(..) => panic!("nothing was cached, the set cannot be complete yet"),
        }
        // A second search while the build runs neither waits nor starts a second build.
        let started = Instant::now();
        let again = hot_set_nowait(&store, tag, embed.clone()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(again, HotAnswer::Partial { .. } | HotAnswer::Complete(..)));

        // Results improve once the build has finished: the complete set appears.
        let deadline = Instant::now() + Duration::from_secs(60);
        let complete = loop {
            match hot_set_nowait(&store, tag, embed.clone()).unwrap() {
                HotAnswer::Complete(hot, source) => break (hot, source),
                HotAnswer::Partial { .. } if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                HotAnswer::Partial { embedded, total, .. } => panic!("the build did not finish: {embedded} of {total}"),
            }
        };
        assert_eq!(complete.0.len(), 41);
        assert_eq!(count.load(Ordering::SeqCst), 41, "every note was embedded exactly once, by one build");
        assert!(!build_in_progress(store.root(), tag));
        set_background_build(false);
    }

    #[test]
    fn without_background_building_a_search_uses_cached_vectors_and_starts_no_build() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        set_background_build(false);
        let store = store_with_notes("nobuild", 20);
        let tag = "nobuild-model|768";
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let embed = slow_embedder(Duration::from_millis(5), count.clone());
        // Part of the notes are cached already (an earlier, interrupted build).
        let notes = store.load_all_notes().unwrap();
        let some: Vec<&Note> = notes.values().take(8).collect();
        let keys: Vec<[u8; 32]> = some.iter().map(|n| memb_embed::document_key(tag, &n.title, &n.desc)).collect();
        let vectors: Vec<QuantizedVec> = some.iter().map(|n| fake_vector(&memb_embed::document_text(&n.title, &n.desc))).collect();
        let entries: Vec<_> = keys.into_iter().zip(vectors).collect();
        store.with_db(|db| memb_embed::put_document_vectors(db, &entries)).unwrap();

        match hot_set_nowait(&store, tag, embed.clone()).unwrap() {
            HotAnswer::Partial { vectors, embedded, total, building } => {
                assert_eq!((vectors.len(), embedded, total, building), (8, 8, 21, false));
            }
            HotAnswer::Complete(..) => panic!("13 notes have no vector"),
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(count.load(Ordering::SeqCst), 0, "a hook or CLI search embeds nothing and starts nothing");
        assert!(!build_in_progress(store.root(), tag));
    }

    #[test]
    fn an_interrupted_build_resumes_without_embedding_the_finished_notes_again() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let store = store_with_notes("resume", 40);
        let tag = "resume-model|768";
        // 41 notes go out in chunks of 16, 16 and 9. The second chunk fails: the build stops there.
        let calls = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let interrupted = hot_set_batch(&store, tag, |notes| {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return None;
            }
            done.fetch_add(notes.len(), Ordering::SeqCst);
            Some(notes.iter().map(|n| fake_vector(&memb_embed::document_text(&n.title, &n.desc))).collect())
        })
        .unwrap();
        assert!(interrupted.is_none(), "an embedding failure leaves no hot set");
        assert_eq!(done.load(Ordering::SeqCst), HOT_CHUNK, "the first chunk was embedded and saved");
        assert!(store.with_db(|db| memb_embed::read_hot_set(db, tag)).unwrap().is_none());

        // The next build only embeds what is still missing.
        let again = AtomicUsize::new(0);
        let (hot, source) = hot_set_batch(&store, tag, |notes| {
            again.fetch_add(notes.len(), Ordering::SeqCst);
            Some(notes.iter().map(|n| fake_vector(&memb_embed::document_text(&n.title, &n.desc))).collect())
        })
        .unwrap()
        .unwrap();
        assert_eq!(hot.len(), 41);
        assert_eq!(source, HotSource::Rebuilt { embedded: 41 - HOT_CHUNK });
        assert_eq!(again.load(Ordering::SeqCst), 41 - HOT_CHUNK, "the finished notes were not embedded again");
    }

    fn fake_vector(text: &str) -> QuantizedVec {
        let mut x = text.bytes().fold(1469598103934665603u64, |h, b| (h ^ b as u64).wrapping_mul(1099511628211));
        let v: Vec<f32> = (0..768)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect();
        QuantizedVec::from_f32(&v)
    }

    #[test]
    fn the_hot_set_is_small_persistent_version_keyed_and_never_re_embeds_unchanged_notes() {
        use memb_core::{Draft, Kind, NotePatch};
        use std::collections::BTreeMap;
        use std::time::Instant;
        let root = std::env::temp_dir().join(format!(
            "memb-rag-hot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(&root).unwrap();
        let draft = |title: String, desc: String, kind: Kind, parents: Vec<u64>| Draft { title, desc, kind, parents, properties: BTreeMap::new() };
        let topic = store.remember(draft("topic".into(), "root".into(), Kind::Topic, vec![])).unwrap();
        let mut ids = vec![topic.id];
        for i in 0..300 {
            ids.push(
                store
                    .remember(draft(format!("note {i}"), format!("description of note {i} about subject {}", i % 37), Kind::Knowledge, vec![topic.id]))
                    .unwrap()
                    .id,
            );
        }
        let tag = "test-model|768";
        let embedded = std::cell::Cell::new(0usize);
        let embed = |n: &Note| {
            embedded.set(embedded.get() + 1);
            Some(fake_vector(&memb_embed::document_text(&n.title, &n.desc)))
        };

        // First use builds the set: every active note is embedded once.
        let (hot, source) = hot_set(&store, tag, embed).unwrap().unwrap();
        assert_eq!(source, HotSource::Rebuilt { embedded: 301 });
        assert_eq!((hot.len(), embedded.get()), (301, 301));
        let bytes = memb_embed::hot_set_bytes(hot.len(), 768);
        eprintln!("T023 hot set: {} notes x 768 dims = {} bytes ({:.2} MB)", hot.len(), bytes, bytes as f64 / 1e6);
        assert!(bytes < 5_000_000, "{bytes}");
        // The persisted row is exactly that size plus its 16-byte header.
        let stored = store
            .with_db(|db| {
                let r = db.begin_read().map_err(|e| e.to_string())?;
                let t = r.open_table(memb_embed::HOT_SET).map_err(|e| e.to_string())?;
                let v = t.get(tag).map_err(|e| e.to_string())?.unwrap();
                Ok(v.value().len())
            })
            .unwrap();
        assert_eq!(stored, 16 + bytes);

        // Unchanged knowledge base: answered from memory, no scan, no embedding.
        let scans = memb_store::full_scan_count();
        let started = Instant::now();
        for _ in 0..200 {
            let (_, source) = hot_set(&store, tag, embed).unwrap().unwrap();
            assert_eq!(source, HotSource::Memory);
        }
        let with_version_check = started.elapsed() / 200;
        let version = store.kb_version().unwrap();
        let started = Instant::now();
        for _ in 0..10_000 {
            assert!(hot_cache_lookup(store.root(), tag, version).is_some());
        }
        let lookup = started.elapsed() / 10_000;
        eprintln!("T023 cache hit: lookup {lookup:?}; with the version check (opens the database) {with_version_check:?}");
        assert!(lookup < std::time::Duration::from_millis(1), "{lookup:?}");
        assert_eq!(memb_store::full_scan_count(), scans, "a hit reads no notes");
        assert_eq!(embedded.get(), 301);

        // After a restart (nothing in memory): read from the persisted row, still no scan or embedding.
        hot_cache().lock().unwrap().clear();
        let started = Instant::now();
        let (_, source) = hot_set(&store, tag, embed).unwrap().unwrap();
        eprintln!("T023 persisted hot set load: {:?}", started.elapsed());
        assert_eq!(source, HotSource::Disk);
        assert_eq!(memb_store::full_scan_count(), scans);
        assert_eq!(embedded.get(), 301);

        // One note changes: the version moves, and exactly that note is embedded again.
        store.update(ids[10], NotePatch { desc: Some("rewritten".into()), ..Default::default() }).unwrap();
        let (_, source) = hot_set(&store, tag, embed).unwrap().unwrap();
        assert_eq!(source, HotSource::Rebuilt { embedded: 1 });
        assert_eq!(embedded.get(), 302);
        // A new note with the same text as an existing one is a content-hash hit, whatever its id.
        let twin = store
            .remember(draft("note 20".into(), "description of note 20 about subject 20".into(), Kind::Knowledge, vec![topic.id]))
            .unwrap();
        let (hot, source) = hot_set(&store, tag, embed).unwrap().unwrap();
        assert_eq!(source, HotSource::Rebuilt { embedded: 0 });
        assert!(hot.iter().any(|(id, _)| *id == twin.id) && hot.len() == 302);
        // A retracted note leaves the set; nothing is embedded.
        store.retract(ids[5], None, false).unwrap();
        let (hot, source) = hot_set(&store, tag, embed).unwrap().unwrap();
        assert_eq!(source, HotSource::Rebuilt { embedded: 0 });
        assert!(hot.iter().all(|(id, _)| *id != ids[5]) && hot.len() == 301);
        // Another model or truncation is a different set with its own vectors.
        let (_, source) = hot_set(&store, "other-model|768", embed).unwrap().unwrap();
        assert_eq!(source, HotSource::Rebuilt { embedded: 301 });
        // A note that cannot be embedded makes the whole set unavailable (the caller goes lexical).
        store.remember(draft("fresh".into(), "unembeddable".into(), Kind::Knowledge, vec![topic.id])).unwrap();
        assert!(hot_set(&store, tag, |_| None).unwrap().is_none());
        // Ranking from the persisted set finds the note by its own vector.
        let (hot, _) = hot_set(&store, tag, embed).unwrap().unwrap();
        let q = fake_vector(&memb_embed::document_text("note 7", "description of note 7 about subject 7"));
        let best = hot.iter().max_by(|a, b| q.cosine(&a.1).total_cmp(&q.cosine(&b.1))).unwrap().0;
        assert_eq!(best, ids[8]);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_loopback_server_without_a_model_identity_is_lexical_only_and_embeds_nothing() {
        let root = std::env::temp_dir().join(format!("memb-rag-nomodel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(&root).unwrap();
        store
            .remember(memb_core::Draft {
                title: "apples".into(),
                desc: "fruit".into(),
                kind: memb_core::Kind::Topic,
                parents: vec![],
                properties: Default::default(),
            })
            .unwrap();
        // A listener that records every connection: lexical-only means nobody connects.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = search_with_server(&store, "apples", port, None).unwrap();
        assert_eq!(found.len(), 1);
        assert!(listener.accept().is_err(), "no embedding request may be sent without a model identity");
        // Control: with a model identity the same call does connect (and then falls back to lexical).
        let found = search_with_server(&store, "apples", port, Some("m")).unwrap();
        assert_eq!(found.len(), 1);
        assert!(listener.accept().is_ok(), "the control must reach the listener, or the check above proves nothing");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A loopback stand-in for Snowflake's SQL API that "embeds" by hashing words into 64
    /// buckets (so related texts get similar vectors) and records every request body.
    fn mock_snowflake() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use std::io::{Read, Write};
        use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let (seen, failing) = (bodies.clone(), fail.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let body: serde_json::Value = text.split("\r\n\r\n").nth(1).and_then(|b| serde_json::from_str(b).ok()).unwrap_or_default();
                if failing.load(Ordering::SeqCst) {
                    let _ = stream.write_all(b"HTTP/1.1 500 X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    continue;
                }
                let mut rows = Vec::new();
                let count = body["bindings"].as_object().map_or(0, |b| b.len());
                for i in 1..=count {
                    let text = body["bindings"][i.to_string()]["value"].as_str().unwrap_or("");
                    let text = text.strip_prefix("Represent this sentence for searching relevant passages: ").unwrap_or(text);
                    let mut v = vec![0f32; 64];
                    for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                        let bucket = word.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) % 64;
                        v[bucket as usize] += 1.0;
                    }
                    rows.push(serde_json::json!([serde_json::to_string(&v).unwrap()]));
                }
                seen.lock().unwrap().push(body);
                let reply = serde_json::json!({"data": rows}).to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        (port, bodies, fail)
    }

    #[test]
    fn snowflake_search_embeds_notes_in_batches_once_and_falls_back_to_lexical_when_it_fails() {
        use memb_core::{Draft, Kind, NotePatch};
        use std::collections::BTreeMap;
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let keys = ["MEMB_EMBED_DISABLE", "MEMB_GEMMA_MODEL_DIR", "MEMB_EMBED_PORT", "MEMB_EMBED_MODEL_ID", "MEMB_EMBED_MRL_DIM", "MEMB_SNOWFLAKE_ACCOUNT", "MEMB_SNOWFLAKE_USER", "MEMB_SNOWFLAKE_ROLE", "MEMB_SNOWFLAKE_WAREHOUSE", "MEMB_SNOWFLAKE_MODEL", "MEMB_SNOWFLAKE_KEY", "MEMB_SNOWFLAKE_BASE_URL"];
        let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        let root = std::env::temp_dir().join(format!("memb-rag-snowflake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = root.join("key.p8");
        std::fs::write(&key, include_str!("../../memb-embed/tests/fixtures/snowflake_test_key.p8")).unwrap();
        let store = Store::open(&root.join("data")).unwrap();
        let topics = ["scarlet apples", "database locks", "release gate", "windows tasks"];
        let mut ids = Vec::new();
        for i in 0..20 {
            let title = if i < 4 { topics[i].to_string() } else { format!("filler note {i}") };
            let desc = if i < 4 { format!("all about {}", topics[i]) } else { format!("unrelated filler text number {i}") };
            ids.push(store.remember(Draft { title, desc, kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() }).unwrap().id);
        }
        let (port, bodies, fail) = mock_snowflake();
        std::env::set_var("MEMB_SNOWFLAKE_ACCOUNT", "ab14212.ap-southeast-1");
        std::env::set_var("MEMB_SNOWFLAKE_USER", "SOMEONE");
        std::env::set_var("MEMB_SNOWFLAKE_WAREHOUSE", "WH");
        std::env::set_var("MEMB_SNOWFLAKE_KEY", &key);
        std::env::set_var("MEMB_SNOWFLAKE_BASE_URL", format!("http://127.0.0.1:{port}"));

        // The first search answers at once from lexical search (one statement, for the query);
        // the 20 notes (16 + 4) are embedded by the background build in two more statements.
        set_background_build(true);
        let _ = search(&store, "scarlet apples").unwrap();
        wait_for_builds();
        let found = search(&store, "scarlet apples").unwrap();
        assert_eq!(found.first().map(|n| n.id), Some(ids[0]), "the vector match comes first: {found:?}");
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), 3, "query + 2 note batches");
        let query_text = sent[0]["bindings"]["1"]["value"].as_str().unwrap();
        assert_eq!(query_text, "Represent this sentence for searching relevant passages: scarlet apples", "arctic-embed-m takes the retrieval prefix");
        assert!(sent[0]["statement"].as_str().unwrap().contains("AI_EMBED('snowflake-arctic-embed-m-v1.5'"));
        let doc_texts: Vec<&str> = sent[1..].iter().flat_map(|b| b["bindings"].as_object().unwrap().values().map(|v| v["value"].as_str().unwrap())).collect();
        assert_eq!(doc_texts.len(), 20);
        assert!(doc_texts.contains(&"scarlet apples: all about scarlet apples"), "documents go out as `title: desc`");
        // A different query reuses the stored vectors: one statement.
        let found = search(&store, "release gate").unwrap();
        assert_eq!(found.first().map(|n| n.id), Some(ids[2]));
        assert_eq!(bodies.lock().unwrap().len(), 4);
        // The same query again is served from the query cache: no statement at all.
        search(&store, "release gate").unwrap();
        assert_eq!(bodies.lock().unwrap().len(), 4);
        // One edited note: its vector alone is embedded again (query + a batch of one).
        store.update(ids[3], NotePatch { desc: Some("tasks that run on windows".into()), ..Default::default() }).unwrap();
        search(&store, "windows tasks").unwrap();
        wait_for_builds();
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), 6);
        assert_eq!(sent[5]["bindings"].as_object().unwrap().len(), 1);
        // The tag names the model and prompt policy, so it never mixes with other vectors.
        let (tag, _) = embed_text("anything", true).unwrap().unwrap();
        assert_eq!(tag, "snowflake|snowflake-arctic-embed-m-v1.5|prompt:retrieval-prefix-v1");
        assert!(embed_note("t", "d").unwrap().is_some());
        // When Snowflake fails the answer is still the lexical one.
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let found = search(&store, "database locks").unwrap();
        assert_eq!(found.first().map(|n| n.id), Some(ids[1]), "lexical fallback");
        // Disabling embeddings sends nothing to Snowflake.
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        let before = bodies.lock().unwrap().len();
        std::env::set_var("MEMB_EMBED_DISABLE", "1");
        search(&store, "windows tasks").unwrap();
        std::env::remove_var("MEMB_EMBED_DISABLE");
        assert_eq!(bodies.lock().unwrap().len(), before, "disabled means disabled");
        // A partial setup is an error from the embedding functions, not a silent fallback.
        std::env::remove_var("MEMB_SNOWFLAKE_USER");
        assert!(embed_text("x", true).is_err());
        set_background_build(false);
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn embed_text_is_none_without_a_usable_backend_and_rejects_a_bad_truncation() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let keys = ["MEMB_EMBED_DISABLE", "MEMB_GEMMA_MODEL_DIR", "MEMB_EMBED_PORT", "MEMB_EMBED_MODEL_ID", "MEMB_EMBED_MRL_DIM"];
        let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        // No backend configured: lexical-only, not an error.
        assert!(embed_text("hello", false).unwrap().is_none());
        // A port without a model identity cannot produce a tagged vector, and nothing is sent.
        std::env::set_var("MEMB_EMBED_PORT", "1");
        assert!(embed_text("hello", false).unwrap().is_none());
        // Disabled wins over everything.
        std::env::set_var("MEMB_EMBED_MODEL_ID", "m");
        std::env::set_var("MEMB_EMBED_DISABLE", "1");
        assert!(embed_text("hello", false).unwrap().is_none());
        // An unsupported truncation is an error rather than a silently different vector.
        std::env::remove_var("MEMB_EMBED_DISABLE");
        std::env::set_var("MEMB_EMBED_MRL_DIM", "999");
        assert!(embed_text("hello", false).is_err());
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// T036 evidence: the real model on 100 notes. A search must answer at once while the
    /// vector index builds in the background, and answer from vectors once it is done.
    /// `cargo test --release -p memb-rag real_model_hot_set -- --ignored --nocapture`
    #[test]
    #[ignore = "requires MEMB_TEST_GEMMA_DIR and MEMB_TEST_SCRATCH"]
    fn real_model_hot_set_builds_in_the_background_on_100_notes() {
        use std::time::Instant;
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::path::PathBuf::from(std::env::var_os("MEMB_TEST_GEMMA_DIR").unwrap());
        let root = std::path::PathBuf::from(std::env::var_os("MEMB_TEST_SCRATCH").unwrap())
            .join(format!("hot-100-{}", std::process::id()));
        assert!(!root.exists());
        let store = Store::open(&root).unwrap();
        let draft = |title: String, desc: String| Draft { title, desc, kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() };
        let target = store.remember(draft("scarlet apples".into(), "fresh red fruit from the orchard".into())).unwrap();
        for i in 0..99 {
            store.remember(draft(format!("database lock note {i}"), format!("how write lock number {i} is taken and released by the store"))).unwrap();
        }
        // The model load is not what is measured here (a hook or the MCP server pays it elsewhere).
        let started = Instant::now();
        gemma_embed(&directory, "warm up", true).unwrap();
        eprintln!("T036 model load + first query embedding: {:?}", started.elapsed());

        set_background_build(true);
        let started = Instant::now();
        let first = search_with_gemma(&store, "bright red apple", directory.clone()).unwrap();
        let first_took = started.elapsed();
        eprintln!("T036 first search (index missing, 100 notes): {first_took:?}, {} results, build running: {}", first.len(), builds_in_progress().lock().unwrap().len() == 1);
        assert!(first_took < std::time::Duration::from_secs(1), "the first search must not wait for the build: {first_took:?}");
        let started = Instant::now();
        wait_for_builds_within(600);
        eprintln!("T036 background build of 100 notes: {:?}", started.elapsed());
        set_background_build(false);
        let started = Instant::now();
        let after = search_with_gemma(&store, "bright red apple", directory).unwrap();
        eprintln!("T036 search after the build: {:?}, first result id {} (target {})", started.elapsed(), after[0].id, target.id);
        assert_eq!(after[0].id, target.id, "the vector match comes first once the index is complete");
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires MEMB_TEST_GEMMA_DIR and MEMB_TEST_SCRATCH"]
    fn real_gemma_synthetic_retrieval_and_fallback() {
        let root = std::path::PathBuf::from(std::env::var_os("MEMB_TEST_SCRATCH").unwrap())
            .join(format!("gemma-retrieval-{}", std::process::id()));
        assert!(!root.exists());
        let store = Store::open(&root).unwrap();
        let note = store
            .remember(Draft {
                title: "scarlet apples".into(),
                desc: "fresh fruit".into(),
                kind: Kind::Topic,
                parents: vec![],
                properties: BTreeMap::new(),
            })
            .unwrap();
        assert!(hybrid_search(&store, "bright red apple", None, &[])
            .unwrap()
            .is_empty());
        let directory = std::path::PathBuf::from(std::env::var_os("MEMB_TEST_GEMMA_DIR").unwrap());
        // A search answers at once; the vector of the note is made by the background build.
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        set_background_build(true);
        let _ = search_with_gemma(&store, "bright red apple", directory.clone()).unwrap();
        wait_for_builds();
        set_background_build(false);
        assert!(search_with_gemma(&store, "bright red apple", directory)
            .unwrap()
            .iter()
            .any(|n| n.id == note.id));
        assert_eq!(
            search_with_gemma(&store, "apples", root.join("missing-model")).unwrap()[0].id,
            note.id
        );
        // The cached document vector must have the configured Matryoshka width
        // (MEMB_EMBED_MRL_DIM, default full 768) and live under a size-specific identity.
        let mrl = memb_embed::mrl_dim_from_env().unwrap();
        let (tag, _) = memb_embed::gemma::GemmaEmbedder::new(
            std::path::PathBuf::from(std::env::var_os("MEMB_TEST_GEMMA_DIR").unwrap()),
        )
        .embed_tagged("x", true)
        .unwrap();
        let tag = memb_embed::mrl_tag(&tag, mrl);
        let key = memb_embed::document_key(&tag, &note.title, &note.desc);
        let cached = store
            .with_db(|db| memb_embed::get_document_vectors(db, &[key]))
            .unwrap();
        assert_eq!(cached[&key].data.len(), mrl.unwrap_or(768));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Opt-in integration test. Only synthetic content is sent to the child;
    /// the executable, model and scratch directory must be provided explicitly.
    #[test]
    #[ignore = "requires explicitly supplied local model and server"]
    fn real_local_model_synthetic_retrieval() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let exe = std::env::var_os("MEMB_TEST_SERVER").expect("MEMB_TEST_SERVER");
        let model = std::env::var_os("MEMB_TEST_MODEL").expect("MEMB_TEST_MODEL");
        let scratch = std::path::PathBuf::from(
            std::env::var_os("MEMB_TEST_SCRATCH").expect("MEMB_TEST_SCRATCH"),
        );
        assert!(scratch.is_absolute() && scratch.is_dir());
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut child = ChildGuard(
            Command::new(exe)
                .args([
                    "--embedding",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                    "--model",
                ])
                .arg(model)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        let q = loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "server exited during startup"
            );
            if let Ok(v) = memb_embed::local_embedding(port, "bright red apple") {
                break v;
            }
            assert!(
                Instant::now() < deadline,
                "server did not return a usable embedding"
            );
            std::thread::sleep(Duration::from_millis(250));
        };
        assert!(!q.data.is_empty());
        let root = scratch.join(format!("memb-real-model-{}", std::process::id()));
        assert!(!root.exists());
        let store = Store::open(&root).unwrap();
        let note = store
            .remember(Draft {
                title: "scarlet apples".into(),
                desc: "fresh fruit".into(),
                kind: Kind::Topic,
                parents: vec![],
                properties: BTreeMap::new(),
            })
            .unwrap();
        assert!(hybrid_search(&store, "bright red apple", None, &[])
            .unwrap()
            .is_empty());
        let found = search_with_server(
            &store,
            "bright red apple",
            port,
            Some("synthetic-integration-model-v1"),
        )
        .unwrap();
        assert!(
            found.iter().any(|n| n.id == note.id),
            "real-model vector-only result missing"
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn server_indexes_all_notes_and_invalidates_durable_cache() {
        use std::io::{Read, Write};
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let (stopped, calls) = (stop.clone(), count.clone());
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..pos]);
                        let len: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                calls.fetch_add(1, Ordering::Relaxed);
                let text = String::from_utf8(bytes).unwrap();
                let body = if text.contains("semanticquery") || text.contains("Target vector") {
                    r#"{"embedding":[1.0,0.0]}"#
                } else {
                    r#"{"embedding":[0.0,1.0]}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let dir = std::env::temp_dir().join(format!(
            "memb-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&dir).unwrap();
        let mut target = 0;
        for i in 0..65 {
            let title = if i == 64 {
                "Target vector".to_owned()
            } else {
                format!("Other {i}")
            };
            target = store
                .remember(Draft {
                    title,
                    desc: "synthetic".into(),
                    kind: Kind::Topic,
                    parents: vec![],
                    properties: BTreeMap::new(),
                })
                .unwrap()
                .id;
        }
        // A search answers at once and the notes are embedded by the background build.
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        set_background_build(true);
        let _ = search_with_server(&store, "semanticquery", port, Some("model-v1")).unwrap();
        wait_for_builds();
        let results = search_with_server(&store, "semanticquery", port, Some("model-v1")).unwrap();
        assert_eq!(results[0].id, target);
        assert_eq!(count.load(Ordering::Relaxed), 66);
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            search_with_server(&store, "semanticquery", port, Some("model-v1")).unwrap()[0].id,
            target
        );
        assert_eq!(count.load(Ordering::Relaxed), 66);
        store
            .update(
                target,
                memb_core::NotePatch {
                    desc: Some("synthetic changed".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let _ = search_with_server(&store, "semanticquery", port, Some("model-v1")).unwrap();
        wait_for_builds();
        assert_eq!(
            search_with_server(&store, "semanticquery", port, Some("model-v1")).unwrap()[0].id,
            target
        );
        assert_eq!(count.load(Ordering::Relaxed), 67);
        let _ = search_with_server(&store, "semanticquery", port, Some("model-v2")).unwrap();
        wait_for_builds();
        assert_eq!(
            search_with_server(&store, "semanticquery", port, Some("model-v2")).unwrap()[0].id,
            target
        );
        assert_eq!(count.load(Ordering::Relaxed), 133);
        set_background_build(false);
        drop(store);
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// Notes whose vector scores at or below the floor are dropped; the floor and top_k come
    /// from the environment (set by `[retrieval]` in config.toml) and fall back to the defaults.
    #[test]
    fn notes_below_the_score_floor_are_dropped_and_top_k_limits_the_seeds() {
        let dir = std::env::temp_dir().join(format!("memb-rag-floor-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::open(&dir).unwrap();
        // Cosine against the query (1, 0) is the first component of a unit vector.
        let make = |title: &str, cosine: f32| {
            let note = store
                .remember(Draft { title: title.into(), desc: "synthetic".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
                .unwrap();
            (note.id, QuantizedVec::from_f32(&[cosine, (1.0 - cosine * cosine).sqrt()]))
        };
        let index = vec![make("near", 0.9), make("middle", 0.5), make("weak", 0.3), make("far", 0.1)];
        let q = QuantizedVec::from_f32(&[1.0, 0.0]);
        let ids = |found: Vec<Note>| found.into_iter().map(|n| n.id).collect::<Vec<_>>();
        let run = |k: usize, f: f32| ids(fuse_with(&store, "zzqq", Some(&q), &index, false, k, f).unwrap());
        assert_eq!(parse_params(None, None, Backend::Gemma), (DEFAULT_TOP_K, GEMMA_SCORE_FLOOR));
        let (k, f) = parse_params(Some("7"), Some("0.42"), Backend::Gemma);
        assert_eq!((k, f), (7, 0.42));
        for junk in [("0", "banana"), ("99", "2.5"), ("-1", "")] {
            assert_eq!(parse_params(Some(junk.0), Some(junk.1), Backend::Gemma), (DEFAULT_TOP_K, GEMMA_SCORE_FLOOR), "{junk:?}");
        }
        let default = run(DEFAULT_TOP_K, GEMMA_SCORE_FLOOR);
        let expected: Vec<u64> = index.iter().filter(|(_, v)| q.cosine(v) > GEMMA_SCORE_FLOOR).map(|(id, _)| *id).collect();
        assert_eq!(default, expected, "the default floor decides");
        assert!(!default.contains(&index[3].0), "a note at 0.1 is below every floor");
        assert_eq!(run(5, 0.6), vec![index[0].0], "0.5 and 0.3 fall below a 0.6 floor");
        assert_eq!(run(5, 0.4), vec![index[0].0, index[1].0], "0.3 falls below a 0.4 floor");
        assert!(run(5, 0.95).is_empty(), "nothing clears 0.95");
        assert_eq!(run(2, 0.0).len(), 2, "top_k = 2 keeps two seeds");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The default floor depends on the backend that made the vectors, an unmeasured backend
    /// is permissive, and an explicit setting beats every default (T045).
    #[test]
    fn the_default_score_floor_follows_the_backend_and_an_explicit_setting_overrides_it() {
        assert_eq!(default_score_floor(Backend::Gemma), 0.60);
        assert_eq!(default_score_floor(Backend::Snowflake), SNOWFLAKE_SCORE_FLOOR);
        // The measured Snowflake scores (right notes 0.239 to 0.577) mean the local floor would
        // drop all of its vector hits: the two defaults must stay different.
        assert!(default_score_floor(Backend::Snowflake) < 0.239 + 0.05 && default_score_floor(Backend::Snowflake) < default_score_floor(Backend::Gemma));
        assert_eq!(default_score_floor(Backend::Loopback), LOOPBACK_SCORE_FLOOR);
        // Only vectors of an unknown origin fall back to the permissive floor.
        assert_eq!(default_score_floor(Backend::Other), UNMEASURED_SCORE_FLOOR);
        assert!(UNMEASURED_SCORE_FLOOR <= GEMMA_SCORE_FLOOR, "an unmeasured backend is never stricter than the measured local one");
        assert!(default_score_floor(Backend::Loopback) <= 0.206 + 0.05, "right notes of the measured loopback server score from 0.206");
        for backend in [Backend::Gemma, Backend::Snowflake, Backend::Loopback, Backend::Other] {
            assert_eq!(parse_params(None, None, backend).1, default_score_floor(backend));
            assert_eq!(parse_params(None, Some("0.33"), backend).1, 0.33, "an explicit floor wins for {backend:?}");
            assert_eq!(parse_params(None, Some("nonsense"), backend).1, default_score_floor(backend), "a junk floor falls back to the backend default");
        }
        // A vector whose cosine is 0.5 survives an unmeasured backend and not the local one.
        let dir = std::env::temp_dir().join(format!("memb-rag-floor-backend-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::open(&dir).unwrap();
        let note = store.remember(Draft { title: "middling".into(), desc: "synthetic".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() }).unwrap();
        let index = vec![(note.id, QuantizedVec::from_f32(&[0.5, (1.0f32 - 0.25).sqrt()]))];
        let q = QuantizedVec::from_f32(&[1.0, 0.0]);
        let kept = |floor: f32| fuse_with(&store, "zzqq", Some(&q), &index, false, 5, floor).unwrap().len();
        assert_eq!(kept(default_score_floor(Backend::Gemma)), 0, "0.5 is below the local floor");
        assert_eq!(kept(default_score_floor(Backend::Loopback)), 1, "0.5 is above the loopback floor");
        assert_eq!(kept(default_score_floor(Backend::Other)), 1, "0.5 is above the permissive unmeasured floor");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hybrid_finds_vector_only_notes_and_falls_back() {
        let dir = std::env::temp_dir().join(format!(
            "memb-rag-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&dir).unwrap();
        let note = store
            .remember(Draft {
                title: "Synthetic fruit".into(),
                desc: "A red apple".into(),
                kind: Kind::Topic,
                parents: vec![],
                properties: BTreeMap::new(),
            })
            .unwrap();
        assert!(hybrid_search(&store, "unrelated", None, &[])
            .unwrap()
            .is_empty());
        let v = QuantizedVec::from_f32(&[1.0, 0.0]);
        let found = hybrid_search(&store, "unrelated", Some(&v), &[(note.id, v.clone())]).unwrap();
        assert_eq!(found[0].id, note.id);
        assert_eq!(
            hybrid_search(&store, "apple", None, &[]).unwrap()[0].id,
            note.id
        );
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(
            search_with_server(&store, "apple", port, Some("unavailable-test")).unwrap()[0].id,
            note.id
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
