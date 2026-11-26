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
    // Supervision is deliberately disabled: reserving a port and then spawning
    // a child cannot establish that the responding listener belongs to it.
    // Never probe or send note content to an unauthenticated child endpoint.
    hybrid_search(store, query, None, &[])
}

type ModelMap = std::sync::Mutex<
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<memb_embed::gemma::GemmaEmbedder>>,
>;

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
        let model = gemma_model(std::path::PathBuf::from(directory)).ok_or("embedding model unavailable")?;
        let (tag, v) = model.embed_tagged(text, is_query)?;
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
        _ => Ok(None),
    }
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
        match embed_text(&memb_embed::document_text(&note.title, &note.desc), false) {
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
    let cached = store
        .with_db(|db| memb_embed::get_document_vectors(db, &keys))
        .unwrap_or_default();
    let mut fresh = Vec::new();
    let mut entries: Vec<(u64, QuantizedVec)> = Vec::new();
    for (note, key) in active.iter().zip(&keys) {
        let vector = match cached.get(key) {
            Some(v) => v.clone(),
            None => match embed_doc(note) {
                Some(v) => {
                    fresh.push((*key, v.clone()));
                    v
                }
                None => return Ok(None),
            },
        };
        if entries.first().is_some_and(|(_, f)| f.data.len() != vector.data.len()) {
            return Ok(None);
        }
        entries.push((note.id, vector));
    }
    if !fresh.is_empty() {
        let _ = store.with_db(|db| memb_embed::put_document_vectors(db, &fresh));
    }
    let _ = store.with_db(|db| memb_embed::write_hot_set(db, tag, version, &entries));
    let hot = std::sync::Arc::new(entries);
    hot_cache_store(store.root(), tag, version, hot.clone());
    Ok(Some((hot, HotSource::Rebuilt { embedded: fresh.len() })))
}

fn search_with_gemma(
    store: &Store,
    query: &str,
    directory: std::path::PathBuf,
) -> Result<Vec<Note>, String> {
    let Some(model) = gemma_model(directory) else {
        return hybrid_search(store, query, None, &[]);
    };
    // An unsupported MEMB_EMBED_MRL_DIM or a failed embedding degrades to lexical search.
    let Ok(mrl) = memb_embed::mrl_dim_from_env() else {
        return hybrid_search(store, query, None, &[]);
    };
    let (tag, q) = match model.embed_tagged(query, true).and_then(|(tag, v)| {
        Ok((memb_embed::mrl_tag(&tag, mrl), memb_embed::apply_mrl(v, mrl)?))
    }) {
        Ok((tag, v)) => (tag, QuantizedVec::from_f32(&v)),
        Err(_) => return hybrid_search(store, query, None, &[]),
    };
    let hot = hot_set(store, &tag, |note| {
        let (doc_tag, v) = model
            .embed_tagged(&memb_embed::document_text(&note.title, &note.desc), false)
            .ok()?;
        let v = memb_embed::apply_mrl(v, mrl).ok()?;
        (memb_embed::mrl_tag(&doc_tag, mrl) == tag && v.len() == q.data.len()).then(|| QuantizedVec::from_f32(&v))
    })?;
    match hot {
        Some((index, _)) => fuse(store, query, Some(&q), &index, true),
        None => hybrid_search(store, query, None, &[]),
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
    let hot = hot_set(store, &tag, |note| {
        memb_embed::local_embedding(port, &memb_embed::server_document_text(&note.title, &note.desc))
            .ok()
            .filter(|v| v.data.len() == query_vec.data.len())
    })?;
    match hot {
        Some((index, _)) => fuse(store, query, Some(&query_vec), &index, true),
        None => hybrid_search(store, query, None, &[]),
    }
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
    fuse(store, query, query_vec, hot_index, false)
}

/// `index_is_active`: every id in `hot_index` is an active note (a hot set is built from
/// active notes at the current version), so no note needs to be read to check it.
fn fuse(
    store: &Store,
    query: &str,
    query_vec: Option<&QuantizedVec>,
    hot_index: &[(u64, QuantizedVec)],
    index_is_active: bool,
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
            .filter(|(s, _)| *s > 0.25)
            .collect();
        vec_scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let vector_ids: Vec<u64> = vec_scored.into_iter().take(10).map(|(_, id)| id).collect();
        // A note retracted after the hot set was built must not take a seed slot.
        let vector_ids = if index_is_active { store.filter_active(&vector_ids)? } else { vector_ids };
        reciprocal_rank_fusion(&lexical_ids, &vector_ids, 5)
    } else {
        lexical_ids.into_iter().take(5).collect()
    };
    store.expand_one_hop(&fused_seeds, 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memb_core::{Draft, Kind};
    use std::collections::BTreeMap;

    /// The environment is process-wide: serialize the tests that change it.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        assert_eq!(
            search_with_server(&store, "semanticquery", port, Some("model-v1")).unwrap()[0].id,
            target
        );
        assert_eq!(count.load(Ordering::Relaxed), 67);
        assert_eq!(
            search_with_server(&store, "semanticquery", port, Some("model-v2")).unwrap()[0].id,
            target
        );
        assert_eq!(count.load(Ordering::Relaxed), 133);
        drop(store);
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
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
