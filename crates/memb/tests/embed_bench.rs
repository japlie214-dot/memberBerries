//! Opt-in benchmark of the in-process EmbeddingGemma 2 model on this machine: speed, memory
//! and retrieval quality. Synthetic text only. Needs an installed model:
//!
//!   MEMB_BENCH_GEMMA_DIR=<model dir> MEMB_BENCH_OUT=<report file> \
//!     cargo test --release -p memb --test embed_bench -- --ignored --nocapture
//!
//! The report is Markdown written to MEMB_BENCH_OUT (and printed).
use memb_core::{Draft, Kind};
use memb_embed::{apply_mrl, gemma::GemmaEmbedder, QuantizedVec};
use memb_store::Store;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

const NOTES: [(&str, &str); 30] = [
    ("Release gate", "Run the unit tests, package build and changelog check before tagging any release."),
    ("Database choice", "We store notes in an embedded single-file database instead of running a separate server process."),
    ("Hook privacy", "Prompts containing credentials are rejected and never written to the conversation log."),
    ("Quota backoff", "When the model provider says the usage limit is reached, wait and retry later using the reset time it reports."),
    ("Memory cap", "The background process is limited to two gigabytes of memory by a Windows job object."),
    ("Idle unload", "The embedding model is released from memory after several minutes without use."),
    ("Query truncation", "Very long search texts keep their beginning and end and drop the middle before embedding."),
    ("Mirror export", "Notes can be copied once into a folder of Markdown files for use in a note-taking app."),
    ("Rollback", "A mistaken change to a note can be undone by restoring its earlier revision."),
    ("Thin scroll pruning", "Short conversations that have been quiet for two weeks are deleted to save space."),
    ("Merge proposals", "Near-duplicate notes are suggested for combining but never merged without approval."),
    ("Daily routine", "A scheduled job runs every afternoon to sweep conversations and prepare curation work."),
    ("Model download", "The language model files are fetched once from a public hub and checked against fixed checksums."),
    ("Windows only", "The product targets Windows desktops and does not support macOS or Linux."),
    ("Stable versions", "Each iteration increases the version number so installed packages can be told apart."),
    ("Hardware tiers", "Weaker computers get a smaller, faster model while stronger ones get a more precise one."),
    ("Vector width", "Embedding vectors can be shortened to 256 dimensions to save space with little loss."),
    ("Cache invalidation", "Cached vectors are rebuilt only when the knowledge base version number changes."),
    ("Claim cap", "Expanding a hit never adds more than four detail statements from one parent note."),
    ("Directive detection", "Phrases like from now on or always mark a conversation chunk as a standing instruction."),
    ("Session ids", "Conversation identifiers may contain only letters, digits, dashes, underscores and dots."),
    ("Antigravity transcript", "For the Google agent CLI the hook reads the tail of its transcript file instead of the payload."),
    ("Typed confirmation", "Applying a proposal requires typing the proposal number so a stray keypress cannot approve it."),
    ("Offline tests", "The whole workspace test suite runs without network access using locked dependencies."),
    ("Evidence packets", "Related conversation summaries are grouped into packets before the model sees them."),
    ("Cooldown", "After a curation run the next one waits twenty hours unless forced by the user."),
    ("Lock file", "Only one process may write the database at a time, enforced by an operating system lock."),
    ("Config validation", "A malformed settings file produces a clear message naming the setting but not its value."),
    ("Kill switch", "Setting one environment variable turns off all conversation capture immediately."),
    ("Snowflake backend", "When the local model cannot run, embeddings can come from a cloud warehouse function."),
];

/// One paraphrased question per note, in the same order (gold = the same index).
const QUERIES: [&str; 30] = [
    "what checks must pass before I publish a build?",
    "why don't we run a database server?",
    "are passwords ever saved from chats?",
    "what happens when the AI service says I've used up my allowance?",
    "how much RAM can the daemon take?",
    "when does the neural text model get freed?",
    "what is done with an overly long search phrase?",
    "can I view my memories in Obsidian?",
    "how do I reverse a bad edit?",
    "which old chats get cleaned up?",
    "does the system automatically combine similar memories?",
    "what runs on a timer each day?",
    "where do the neural network weights come from?",
    "does it work on a Mac?",
    "how can I tell two installed builds apart?",
    "what model does a laptop with little RAM use?",
    "can embeddings be made smaller?",
    "when are stored vectors recomputed?",
    "how many sub-facts can a search result pull in per item?",
    "how does it recognise a rule I want remembered?",
    "what characters are allowed in a chat id?",
    "how does capture work with Gemini's command line agent?",
    "how do I stop an accidental approval?",
    "can tests run on a plane with no internet?",
    "how are summaries clustered before curation?",
    "how often can curation run?",
    "what prevents two writers corrupting the store?",
    "what does a broken config error reveal?",
    "how do I instantly stop recording?",
    "what's the fallback if the on-device embedder is too slow?",
];

fn pct(samples: &mut [f64], p: f64) -> f64 {
    samples.sort_by(|a, b| a.total_cmp(b));
    samples[(((samples.len() - 1) as f64) * p).round() as usize]
}

fn mib() -> String {
    memb_win::process_private_mib().map_or("?".into(), |m| m.to_string())
}

fn words(n: usize) -> String {
    let pool = [
        "the", "release", "process", "stores", "notes", "in", "a", "local", "database", "and", "checks", "every", "change", "before",
        "writing", "it", "so", "that", "later", "searches", "find", "the", "right", "memory", "quickly", "without", "any", "network",
    ];
    (0..n).map(|i| pool[(i * 7 + i / 5) % pool.len()]).collect::<Vec<_>>().join(" ")
}

fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

/// recall@1, @3, @5 and MRR@10 of ranked id lists against one gold id per query.
fn score(ranked: &[Vec<usize>]) -> (f64, f64, f64, f64) {
    let n = ranked.len() as f64;
    let (mut r1, mut r3, mut r5, mut mrr) = (0.0, 0.0, 0.0, 0.0);
    for (gold, list) in ranked.iter().enumerate() {
        if let Some(pos) = list.iter().position(|id| *id == gold) {
            r1 += f64::from(u8::from(pos < 1));
            r3 += f64::from(u8::from(pos < 3));
            r5 += f64::from(u8::from(pos < 5));
            if pos < 10 {
                mrr += 1.0 / (pos as f64 + 1.0);
            }
        }
    }
    (r1 / n, r3 / n, r5 / n, mrr / n)
}

#[test]
#[ignore = "needs MEMB_BENCH_GEMMA_DIR with an installed model"]
fn embeddinggemma_q4_speed_memory_and_retrieval_quality_on_this_machine() {
    let dir = PathBuf::from(std::env::var_os("MEMB_BENCH_GEMMA_DIR").expect("MEMB_BENCH_GEMMA_DIR"));
    let mut report = String::new();
    macro_rules! line {
        ($($arg:tt)*) => {{ let l = format!($($arg)*); println!("{l}"); let _ = writeln!(report, "{l}"); }};
    }
    let hw = memb_win::probe_hardware();
    line!("# EmbeddingGemma 2 (ONNX q4) on this machine\n");
    line!("{} physical cores, {} MiB RAM, AVX-512 {}, AVX2+FMA {}; tier {:?}; MEMB_EMBED_THREADS={}\n", hw.physical_cores, hw.total_ram_mib, hw.avx512, hw.avx2_fma, memb_win::choose_tier(&hw), std::env::var("MEMB_EMBED_THREADS").unwrap_or_else(|_| "unset (2)".into()));

    // ---- load and memory
    let before = mib();
    let model = GemmaEmbedder::new(dir.clone());
    let started = Instant::now();
    let (tag, probe) = model.embed_tagged("warm up", false).expect("first embedding");
    let first = started.elapsed();
    let loaded = mib();
    line!("## Load and memory\n");
    line!("- first embedding (loads the model, ONNX session, tokenizer): {:.0} ms; vector width {}", first.as_secs_f64() * 1000.0, probe.len());
    line!("- process private memory: {before} MiB before, {loaded} MiB with the model loaded");

    // ---- latency by length
    line!("\n## Latency per text (sequential, release build)\n");
    line!("| text | approx tokens | n | p50 ms | p95 ms | max ms |");
    line!("|---|---|---|---|---|---|");
    for (label, n_words, is_query, runs) in [
        ("query, short", 8, true, 40),
        ("query, 100 words", 100, true, 20),
        ("query, 600 words (cut to 512 tokens)", 600, true, 10),
        ("document, one line", 14, false, 40),
        ("document, paragraph", 90, false, 20),
        ("document, page", 400, false, 10),
        ("document, long (1,500 words)", 1500, false, 5),
    ]
    .into_iter()
    .filter(|(_, n, _, _)| std::env::var_os("MEMB_BENCH_QUICK").is_none() || *n <= 100)
    {
        let text = words(n_words);
        let mut ms = Vec::new();
        for _ in 0..runs {
            let t = Instant::now();
            model.embed_tagged(&text, is_query).unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let max = ms.iter().cloned().fold(0.0, f64::max);
        line!("| {label} | {} | {runs} | {:.0} | {:.0} | {max:.0} |", (n_words as f64 * 1.3) as usize, pct(&mut ms.clone(), 0.5), pct(&mut ms, 0.95));
    }
    line!("\n- memory after the latency runs: {} MiB", mib());

    // ---- retrieval quality on the labelled corpus
    let docs: Vec<Vec<f32>> = NOTES
        .iter()
        .map(|(t, d)| model.embed_tagged(&memb_embed::document_text(t, d), false).unwrap().1)
        .collect();
    let queries: Vec<Vec<f32>> = QUERIES.iter().map(|q| model.embed_tagged(q, true).unwrap().1).collect();
    let rank_by = |width: usize, quantized: bool| -> Vec<Vec<usize>> {
        let cut = |v: &Vec<f32>| apply_mrl(v.clone(), Some(width)).unwrap();
        let (d, q): (Vec<_>, Vec<_>) = (docs.iter().map(cut).collect(), queries.iter().map(cut).collect());
        q.iter()
            .map(|qv| {
                let qq = QuantizedVec::from_f32(qv);
                let mut s: Vec<(f32, usize)> = d
                    .iter()
                    .enumerate()
                    .map(|(i, dv)| (if quantized { qq.cosine(&QuantizedVec::from_f32(dv)) } else { cosine_f32(qv, dv) }, i))
                    .collect();
                s.sort_by(|a, b| b.0.total_cmp(&a.0));
                s.into_iter().map(|x| x.1).collect()
            })
            .collect()
    };
    // lexical baseline and the shipped hybrid search, through a real store
    let root = std::env::temp_dir().join(format!("memb-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).unwrap();
    let ids: Vec<u64> = NOTES
        .iter()
        .map(|(t, d)| {
            store
                .remember(Draft { title: (*t).into(), desc: (*d).into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
                .unwrap()
                .id
        })
        .collect();
    let to_idx = |id: u64| ids.iter().position(|x| *x == id).unwrap();
    let lexical: Vec<Vec<usize>> = QUERIES.iter().map(|q| store.search_lexical_ids(q, 10).unwrap_or_default().into_iter().map(to_idx).collect()).collect();

    std::env::set_var("MEMB_GEMMA_MODEL_DIR", &dir);
    std::env::set_var("MEMB_EMBED_MRL_DIM", "256");
    std::env::remove_var("MEMB_EMBED_DISABLE");
    std::env::remove_var("MEMB_EMBED_PORT");
    let t = Instant::now();
    let _ = memb_rag::search(&store, QUERIES[0]).unwrap(); // builds the hot set
    let build = t.elapsed();
    let hybrid: Vec<Vec<usize>> = QUERIES.iter().map(|q| memb_rag::search(&store, q).unwrap().into_iter().map(|n| to_idx(n.id)).collect()).collect();
    let mut warm = Vec::new();
    for q in QUERIES.iter().cycle().take(60) {
        let t = Instant::now();
        memb_rag::search(&store, q).unwrap();
        warm.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let mut lex_ms = Vec::new();
    for q in QUERIES.iter() {
        let t = Instant::now();
        store.search_lexical_ids(q, 10).unwrap();
        lex_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    line!("\n## Retrieval quality (30 notes, 30 paraphrased questions, one right note each)\n");
    line!("| method | recall@1 | recall@3 | recall@5 | MRR@10 |");
    line!("|---|---|---|---|---|");
    let mut row = |name: &str, ranked: &Vec<Vec<usize>>| {
        let (a, b, c, m) = score(ranked);
        line!("| {name} | {a:.2} | {b:.2} | {c:.2} | {m:.2} |");
    };
    row("lexical only (today's default)", &lexical);
    row("vectors, 768-wide, f32 (reference)", &rank_by(768, false));
    row("vectors, 512-wide, f32", &rank_by(512, false));
    row("vectors, 256-wide, f32", &rank_by(256, false));
    row("vectors, 256-wide, i8 (what memb stores)", &rank_by(256, true));
    row("vectors, 128-wide, i8", &rank_by(128, true));
    row("hybrid search as shipped (256-wide i8 + lexical, RRF)", &hybrid);

    line!("\n## End-to-end search through memb (30-note store)\n");
    line!("- first search, building the hot set (30 documents embedded): {:.0} ms", build.as_secs_f64() * 1000.0);
    line!("- warm search (query embedded, hot set in memory): p50 {:.0} ms, p95 {:.0} ms", pct(&mut warm.clone(), 0.5), pct(&mut warm, 0.95));
    line!("- lexical search alone: p50 {:.1} ms", pct(&mut lex_ms, 0.5));

    // ---- scale: a bigger store
    if std::env::var_os("MEMB_BENCH_QUICK").is_some() {
        if let Some(out) = std::env::var_os("MEMB_BENCH_OUT") {
            std::fs::write(out, report).unwrap();
        }
        return;
    }
    let extra = 470;
    for i in 0..extra {
        store
            .remember(Draft {
                title: format!("synthetic note {i}"),
                desc: format!("{} number {i} about subject {}", words(24), i % 41),
                kind: Kind::Topic,
                parents: vec![],
                properties: BTreeMap::new(),
            })
            .unwrap();
    }
    let t = Instant::now();
    let _ = memb_rag::search(&store, QUERIES[3]).unwrap();
    let rebuild = t.elapsed();
    let mut warm500 = Vec::new();
    for q in QUERIES.iter().take(30) {
        let t = Instant::now();
        memb_rag::search(&store, q).unwrap();
        warm500.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    line!("\n## Scale: 500 notes\n");
    line!("- hot set rebuild after adding 470 notes (all new, so 470 embeddings, about 30 words each): {:.1} s, {:.0} ms per document", rebuild.as_secs_f64(), rebuild.as_secs_f64() * 1000.0 / 470.0);
    line!("- hot set size: {} KiB ({} bytes per note at 256 dims)", memb_embed::hot_set_bytes(500, 256) / 1024, memb_embed::hot_set_bytes(1, 256));
    line!("- warm search at 500 notes: p50 {:.0} ms, p95 {:.0} ms", pct(&mut warm500.clone(), 0.5), pct(&mut warm500, 0.95));
    line!("- process private memory now: {} MiB", mib());
    let t = Instant::now();
    let dropped = memb_rag::trim_caches();
    line!("- trim (memory governor / idle unload): dropped {dropped} model(s) in {:.0} ms; memory after: {} MiB", t.elapsed().as_secs_f64() * 1000.0, mib());
    let _ = tag;
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
    if let Some(out) = std::env::var_os("MEMB_BENCH_OUT") {
        std::fs::write(out, report).unwrap();
    }
}

/// Live check of Snowflake AI_EMBED with the user's own account, using only the synthetic
/// corpus above. Needs MEMB_SNOWFLAKE_ACCOUNT, MEMB_SNOWFLAKE_USER, MEMB_SNOWFLAKE_WAREHOUSE and
/// MEMB_SNOWFLAKE_KEY (path to an unencrypted PKCS#8 .p8); optional MEMB_SNOWFLAKE_ROLE and
/// MEMB_SNOWFLAKE_MODELS (comma list). Sends the 30 synthetic notes and questions above, nothing else.
#[test]
#[ignore = "needs Snowflake credentials in MEMB_SNOWFLAKE_*"]
fn snowflake_ai_embed_live_speed_and_retrieval_quality() {
    use memb_embed::snowflake::{Client, SnowflakeConfig};
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let models = var("MEMB_SNOWFLAKE_MODELS").unwrap_or_else(|| "snowflake-arctic-embed-m-v1.5,snowflake-arctic-embed-l-v2.0".into());
    let mut report = String::new();
    macro_rules! line {
        ($($arg:tt)*) => {{ let l = format!($($arg)*); println!("{l}"); let _ = writeln!(report, "{l}"); }};
    }
    line!("# Snowflake AI_EMBED live check (synthetic corpus only)\n");
    line!("| model | query prefix | width | first call ms | single-text p50 ms | batch of 16 ms | recall@1 | recall@3 | recall@5 | MRR@10 | 256-wide recall@1 | 256-wide recall@3 |");
    line!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for model in models.split(',') {
        let cfg = SnowflakeConfig {
            account: var("MEMB_SNOWFLAKE_ACCOUNT").expect("MEMB_SNOWFLAKE_ACCOUNT"),
            user: var("MEMB_SNOWFLAKE_USER").expect("MEMB_SNOWFLAKE_USER"),
            role: var("MEMB_SNOWFLAKE_ROLE"),
            warehouse: var("MEMB_SNOWFLAKE_WAREHOUSE").expect("MEMB_SNOWFLAKE_WAREHOUSE"),
            model: model.trim().to_string(),
            key_path: PathBuf::from(var("MEMB_SNOWFLAKE_KEY").expect("MEMB_SNOWFLAKE_KEY")),
            base_url: None,
        };
        let client = Client::new(cfg).expect("client");
        let t = Instant::now();
        let first = match client.embed_batch(&["hello world".to_string()]) {
            Ok(v) => v,
            Err(e) => {
                line!("| {model} | - | failed: {e} | | | | | | | | | |");
                continue;
            }
        };
        let first_ms = t.elapsed().as_secs_f64() * 1000.0;
        let width = first[0].len();
        let mut singles = Vec::new();
        for i in 0..5 {
            let t = Instant::now();
            client.embed_batch(&[format!("latency probe number {i}")]).unwrap();
            singles.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let t = Instant::now();
        client.embed_batch(&(0..16).map(|i| format!("batch probe {i}")).collect::<Vec<_>>()).unwrap();
        let batch_ms = t.elapsed().as_secs_f64() * 1000.0;
        let docs: Vec<Vec<f32>> = client.embed_batch(&NOTES.iter().map(|(t, d)| format!("{t}: {d}")).collect::<Vec<_>>()).unwrap();
        for prefix in ["", "Represent this sentence for searching relevant passages: "] {
            let queries: Vec<Vec<f32>> = client.embed_batch(&QUERIES.iter().map(|q| format!("{prefix}{q}")).collect::<Vec<_>>()).unwrap();
            let rank = |cut: Option<usize>| -> Vec<Vec<usize>> {
                let prep = |v: &Vec<f32>| match cut {
                    Some(w) => apply_mrl(v.clone(), Some(w)).unwrap(),
                    None => v.clone(),
                };
                queries
                    .iter()
                    .map(|q| {
                        let q = prep(q);
                        let mut s: Vec<(f32, usize)> = docs.iter().enumerate().map(|(i, d)| (cosine_f32(&q, &prep(d)), i)).collect();
                        s.sort_by(|a, b| b.0.total_cmp(&a.0));
                        s.into_iter().map(|x| x.1).collect()
                    })
                    .collect()
            };
            let (a, b, c, m) = score(&rank(None));
            let (a2, b2, _, _) = score(&rank(Some(256)));
            let label = if prefix.is_empty() { "none" } else { "retrieval prefix" };
            line!("| {model} | {label} | {width} | {first_ms:.0} | {:.0} | {batch_ms:.0} | {a:.2} | {b:.2} | {c:.2} | {m:.2} | {a2:.2} | {b2:.2} |", pct(&mut singles.clone(), 0.5));
        }
    }
    if let Some(out) = std::env::var_os("MEMB_BENCH_OUT") {
        std::fs::write(out, report).unwrap();
    }
}

/// The same synthetic corpus through memb's real search path with Snowflake as the backend
/// (config from MEMB_SNOWFLAKE_*): first search (index build), warm searches, and hybrid recall.
#[test]
#[ignore = "needs Snowflake credentials in MEMB_SNOWFLAKE_*"]
fn snowflake_through_memb_search_live() {
    let root = std::env::temp_dir().join(format!("memb-bench-sf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).unwrap();
    let ids: Vec<u64> = NOTES
        .iter()
        .map(|(t, d)| store.remember(Draft { title: (*t).into(), desc: (*d).into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() }).unwrap().id)
        .collect();
    let to_idx = |id: u64| ids.iter().position(|x| *x == id).unwrap();
    std::env::remove_var("MEMB_GEMMA_MODEL_DIR");
    std::env::remove_var("MEMB_EMBED_PORT");
    std::env::remove_var("MEMB_EMBED_DISABLE");
    let t = Instant::now();
    memb_rag::search(&store, QUERIES[0]).unwrap();
    println!("first search (30 notes embedded in 2 statements + query): {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let mut warm = Vec::new();
    let mut ranked = Vec::new();
    for q in QUERIES.iter() {
        let t = Instant::now();
        let found = memb_rag::search(&store, q).unwrap();
        warm.push(t.elapsed().as_secs_f64() * 1000.0);
        ranked.push(found.into_iter().map(|n| to_idx(n.id)).collect::<Vec<_>>());
    }
    let (a, b, c, m) = score(&ranked);
    println!("hybrid through memb (default model, 768-wide i8 + lexical): recall@1 {a:.2} @3 {b:.2} @5 {c:.2} MRR {m:.2}");
    println!("warm search (one query statement): p50 {:.0} ms, p95 {:.0} ms", pct(&mut warm.clone(), 0.5), pct(&mut warm, 0.95));
    drop(store);
    let _ = std::fs::remove_dir_all(root);
}
