use memb_core::Note;
use memb_embed::{reciprocal_rank_fusion, QuantizedVec};
use memb_store::Store;

/// Embeddings are opt-in. Set MEMB_EMBED_PORT for a loopback server and
/// MEMB_EMBED_MODEL_ID to a stable model/version identifier to enable caches.
pub fn search(store: &Store, query: &str) -> Result<Vec<Note>, String> {
    store.search_lexical_ids(query, 1)?;
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

/// One loaded EmbeddingGemma 2 model per directory for the life of the process.
fn gemma_model(
    directory: std::path::PathBuf,
) -> Option<std::sync::Arc<memb_embed::gemma::GemmaEmbedder>> {
    use std::sync::{Arc, Mutex, OnceLock};
    static MODELS: OnceLock<
        Mutex<std::collections::HashMap<std::path::PathBuf, Arc<memb_embed::gemma::GemmaEmbedder>>>,
    > = OnceLock::new();
    let mut models = MODELS.get_or_init(Default::default).lock().ok()?;
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
            let v = memb_embed::local_embedding(port, text)?;
            Ok(Some((memb_embed::mrl_tag(&model, mrl), v)))
        }
        _ => Ok(None),
    }
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
    let cached = store
        .with_db(|db| memb_embed::read_document_cache(db, &tag))
        .unwrap_or_default();
    let notes = store.load_all_notes()?;
    let mut fresh = Vec::new();
    let mut index = Vec::new();
    for note in notes
        .values()
        .filter(|n| n.status == memb_core::NoteStatus::Active)
    {
        let fingerprint = memb_embed::document_fingerprint(&note.title, &note.desc);
        let vector = match cached
            .get(&note.id)
            .filter(|(f, v)| *f == fingerprint && v.data.len() == q.data.len())
        {
            Some((_, v)) => v.clone(),
            None => match model
                .embed_tagged(&format!("{}\n{}", note.title, note.desc), false)
                .and_then(|(t, v)| {
                    Ok((memb_embed::mrl_tag(&t, mrl), memb_embed::apply_mrl(v, mrl)?))
                }) {
                Ok((doc_tag, v)) if doc_tag == tag && v.len() == q.data.len() => {
                    let v = QuantizedVec::from_f32(&v);
                    fresh.push((note.id, fingerprint, v.clone()));
                    v
                }
                _ => return hybrid_search(store, query, None, &[]),
            },
        };
        index.push((note.id, vector));
    }
    if !fresh.is_empty() {
        let _ = store.with_db(|db| memb_embed::write_document_cache(db, &tag, &fresh));
    }
    hybrid_search(store, query, Some(&q), &index)
}

fn search_with_server(
    store: &Store,
    query: &str,
    port: u16,
    model: Option<&str>,
) -> Result<Vec<Note>, String> {
    use std::sync::{Mutex, OnceLock};
    static QUERIES: OnceLock<Mutex<memb_embed::QueryLru>> = OnceLock::new();
    store.search_lexical_ids(query, 1)?;
    let Ok(mrl) = memb_embed::mrl_dim_from_env() else {
        return hybrid_search(store, query, None, &[]);
    };
    // The truncation size is part of the identity of every cached vector.
    let model = model.map(|m| memb_embed::mrl_tag(m, mrl));
    let model = model.as_deref();
    let cache =
        QUERIES.get_or_init(|| Mutex::new(memb_embed::QueryLru::new(memb_embed::QUERY_LRU_CAP)));
    let key = model.map(|m| format!("{port}:{m}:{}:{query}", query.len()));
    let cached = key
        .as_ref()
        .and_then(|k| cache.lock().ok().and_then(|mut c| c.get(k).cloned()));
    let query_vec = match cached.or_else(|| memb_embed::local_embedding(port, query).ok()) {
        Some(v) => v,
        None => return hybrid_search(store, query, None, &[]),
    };
    if let Some(key) = key {
        if let Ok(mut c) = cache.lock() {
            c.insert(key, query_vec.clone());
        }
    }
    let notes = store.load_all_notes()?;
    let cached_docs = model
        .and_then(|m| {
            store
                .with_db(|db| memb_embed::read_document_cache(db, m))
                .ok()
        })
        .unwrap_or_default();
    let mut fresh = Vec::new();
    let mut index = Vec::new();
    for note in notes
        .values()
        .filter(|n| n.status == memb_core::NoteStatus::Active)
    {
        let fingerprint = memb_embed::document_fingerprint(&note.title, &note.desc);
        let cached = cached_docs
            .get(&note.id)
            .filter(|(f, v)| *f == fingerprint && v.data.len() == query_vec.data.len())
            .map(|(_, v)| v.clone());
        let vector = match cached {
            Some(v) => v,
            None => {
                match memb_embed::local_embedding(port, &format!("{}\n{}", note.title, note.desc)) {
                    Ok(v) if v.data.len() == query_vec.data.len() => {
                        fresh.push((note.id, fingerprint, v.clone()));
                        v
                    }
                    _ => return hybrid_search(store, query, None, &[]),
                }
            }
        };
        index.push((note.id, vector));
    }
    if let Some(model) = model {
        if !fresh.is_empty() {
            let _ = store.with_db(|db| memb_embed::write_document_cache(db, model, &fresh));
        }
    }
    hybrid_search(store, query, Some(&query_vec), &index)
}

/// Hybrid retrieval combining lexical ranking + quantized vector scan + RRF (k=60) + 1-hop graph expansion.
/// Gracefully degrades to pure lexical search when `query_vec` is None (e.g. llama-server unavailable).
pub fn hybrid_search(
    store: &Store,
    query: &str,
    query_vec: Option<&QuantizedVec>,
    hot_index: &[(u64, QuantizedVec)],
) -> Result<Vec<Note>, String> {
    let lexical_ids = store.search_lexical_ids(query, 10)?;
    let fused_seeds = if let Some(qv) = query_vec {
        let all = store.load_all_notes()?;
        let mut vec_scored: Vec<(f32, u64)> = hot_index
            .iter()
            .filter(|(id, _)| {
                all.get(id)
                    .is_some_and(|n| n.status == memb_core::NoteStatus::Active)
            })
            .map(|(id, nv)| (qv.cosine(nv), *id))
            .filter(|(s, _)| *s > 0.25)
            .collect();
        vec_scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let vector_ids: Vec<u64> = vec_scored.into_iter().take(10).map(|(_, id)| id).collect();
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
        let cached = store
            .with_db(|db| memb_embed::read_document_cache(db, &memb_embed::mrl_tag(&tag, mrl)))
            .unwrap();
        assert_eq!(cached[&note.id].1.data.len(), mrl.unwrap_or(768));
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
