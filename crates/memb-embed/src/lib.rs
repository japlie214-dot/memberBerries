pub mod gemma;
pub mod idle;

use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

pub const QUERY_LRU_CAP: usize = 256;
/// Document vectors, keyed by a stable content hash (see `document_key`).
pub const DOC_EMBED_CACHE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("doc_embed_cache_v3");
/// The persistent i8 hot working set: one row per model tag (see `write_hot_set`).
pub const HOT_SET: TableDefinition<&str, &[u8]> = TableDefinition::new("hot_set_v1");
/// Earlier cache layouts (keyed by note id, content hash from `DefaultHasher`, whose output
/// is not guaranteed stable between Rust releases). They are dropped when the new table is written.
const OLD_DOC_CACHE_V1: TableDefinition<u64, &[u8]> = TableDefinition::new("doc_embed_cache_v1");
const OLD_DOC_CACHE_V2: TableDefinition<u64, &[u8]> = TableDefinition::new("doc_embed_cache_v2");

/// SHA-256 of a file, streamed (the model files are hundreds of megabytes), as lower-case hex.
pub fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// The text a note is embedded as: with the document prefix this is
/// `title: <title> | text: <desc>`, the EmbeddingGemma retrieval-document format.
pub fn document_text(title: &str, desc: &str) -> String {
    format!("{title} | text: {desc}")
}

/// The longest query sent to a loopback server, in characters (about 512 tokens, the same
/// budget the in-process model applies); longer queries keep their head and tail.
pub const SERVER_QUERY_MAX_CHARS: usize = 2048;

/// The text sent to a loopback server for a query: the retrieval prefix plus the capped query.
pub fn server_query_text(query: &str) -> String {
    let chars: Vec<char> = query.chars().collect();
    let capped: String = if chars.len() > SERVER_QUERY_MAX_CHARS {
        let head = SERVER_QUERY_MAX_CHARS * 30 / 100;
        let tail = SERVER_QUERY_MAX_CHARS - head;
        chars[..head].iter().chain(&chars[chars.len() - tail..]).collect()
    } else {
        query.to_owned()
    };
    format!("{}{capped}", gemma::QUERY_PREFIX)
}

/// The text sent to a loopback server for a document.
pub fn server_document_text(title: &str, desc: &str) -> String {
    format!("{}{}", gemma::DOC_PREFIX, document_text(title, desc))
}

/// A cache key that is stable across runs, machines and Rust releases: SHA-256 over the
/// model identity (which includes the truncation size) and the exact title and description.
/// Equal content with the same model shares one vector, whatever the note id.
pub fn document_key(model_tag: &str, title: &str, desc: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [model_tag, title, desc] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().into()
}

fn encode_vec(v: &QuantizedVec) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + v.data.len());
    out.extend_from_slice(&v.scale.to_le_bytes());
    out.extend(v.data.iter().map(|&b| b as u8));
    out
}

fn decode_vec(bytes: &[u8]) -> Option<QuantizedVec> {
    if bytes.len() <= 4 {
        return None;
    }
    let scale = f32::from_le_bytes(bytes[..4].try_into().ok()?);
    Some(QuantizedVec { scale, data: bytes[4..].iter().map(|&b| b as i8).collect() })
}

/// Point lookups of cached document vectors (no scan of the table).
pub fn get_document_vectors(
    db: &Database,
    keys: &[[u8; 32]],
) -> Result<HashMap<[u8; 32], QuantizedVec>, String> {
    let read = db.begin_read().map_err(|e| e.to_string())?;
    let table = match read.open_table(DOC_EMBED_CACHE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut found = HashMap::new();
    for key in keys {
        if let Some(value) = table.get(key.as_slice()).map_err(|e| e.to_string())? {
            if let Some(v) = decode_vec(value.value()) {
                found.insert(*key, v);
            }
        }
    }
    Ok(found)
}

/// Stores freshly embedded document vectors, and drops the older cache layouts.
pub fn put_document_vectors(db: &Database, entries: &[([u8; 32], QuantizedVec)]) -> Result<(), String> {
    let write = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = write.open_table(DOC_EMBED_CACHE).map_err(|e| e.to_string())?;
        for (key, vector) in entries {
            table
                .insert(key.as_slice(), encode_vec(vector).as_slice())
                .map_err(|e| e.to_string())?;
        }
    }
    let _ = write.delete_table(OLD_DOC_CACHE_V1);
    let _ = write.delete_table(OLD_DOC_CACHE_V2);
    write.commit().map_err(|e| e.to_string())
}

/// The bytes a hot set of `n` vectors of `dim` dimensions takes (ids, scales and i8 data).
pub fn hot_set_bytes(n: usize, dim: usize) -> usize {
    n * (8 + 4 + dim)
}

/// Persists the working set for one model tag: every active note's i8 vector, valid for
/// exactly one `project_kb_version`. Stored as one flat row: version, count, dimension,
/// then all ids, all scales, all data.
pub fn write_hot_set(
    db: &Database,
    tag: &str,
    version: u64,
    entries: &[(u64, QuantizedVec)],
) -> Result<(), String> {
    let dim = entries.first().map(|(_, v)| v.data.len()).unwrap_or(0);
    if entries.iter().any(|(_, v)| v.data.len() != dim) {
        return Err("hot set vectors differ in size".into());
    }
    let mut out = Vec::with_capacity(16 + hot_set_bytes(entries.len(), dim));
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&(dim as u32).to_le_bytes());
    for (id, _) in entries {
        out.extend_from_slice(&id.to_le_bytes());
    }
    for (_, v) in entries {
        out.extend_from_slice(&v.scale.to_le_bytes());
    }
    for (_, v) in entries {
        out.extend(v.data.iter().map(|&b| b as u8));
    }
    let write = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = write.open_table(HOT_SET).map_err(|e| e.to_string())?;
        table.insert(tag, out.as_slice()).map_err(|e| e.to_string())?;
    }
    write.commit().map_err(|e| e.to_string())
}

/// The persisted working set for `tag` and the `project_kb_version` it was built at.
/// A missing or malformed row is `None` (the caller rebuilds it).
#[allow(clippy::type_complexity)]
pub fn read_hot_set(db: &Database, tag: &str) -> Result<Option<(u64, Vec<(u64, QuantizedVec)>)>, String> {
    let read = db.begin_read().map_err(|e| e.to_string())?;
    let table = match read.open_table(HOT_SET) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let Some(row) = table.get(tag).map_err(|e| e.to_string())? else { return Ok(None) };
    let b = row.value();
    if b.len() < 16 {
        return Ok(None);
    }
    let version = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    let dim = u32::from_le_bytes(b[12..16].try_into().unwrap()) as usize;
    let Some(total) = n.checked_mul(8 + 4 + dim) else { return Ok(None) };
    if b.len() != 16 + total {
        return Ok(None);
    }
    let ids = &b[16..16 + n * 8];
    let scales = &b[16 + n * 8..16 + n * 12];
    let data = &b[16 + n * 12..];
    let entries = (0..n)
        .map(|i| {
            (
                u64::from_le_bytes(ids[i * 8..i * 8 + 8].try_into().unwrap()),
                QuantizedVec {
                    scale: f32::from_le_bytes(scales[i * 4..i * 4 + 4].try_into().unwrap()),
                    data: data[i * dim..(i + 1) * dim].iter().map(|&x| x as i8).collect(),
                },
            )
        })
        .collect();
    Ok(Some((version, entries)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantizedVec {
    pub scale: f32,
    pub data: Vec<i8>,
}

impl QuantizedVec {
    pub fn from_f32(v: &[f32]) -> Self {
        let max_abs = v.iter().map(|x| x.abs()).fold(0.0f32, f32::max).max(1e-8);
        let scale = max_abs / 127.0;
        let data = v
            .iter()
            .map(|&x| (x / scale).round().clamp(-127.0, 127.0) as i8)
            .collect();
        Self { scale, data }
    }

    pub fn cosine(&self, other: &Self) -> f32 {
        if self.data.len() != other.data.len() || self.data.is_empty() {
            return 0.0;
        }
        let (mut dot, mut na, mut nb) = (0i64, 0i64, 0i64);
        for (&a, &b) in self.data.iter().zip(&other.data) {
            let (ai, bi) = (a as i64, b as i64);
            dot += ai * bi;
            na += ai * ai;
            nb += bi * bi;
        }
        let denom = ((na as f64).sqrt() * (nb as f64).sqrt()) as f32;
        if denom <= 1e-8 {
            0.0
        } else {
            (dot as f32) / denom
        }
    }
}

pub struct QueryLru {
    cap: usize,
    order: VecDeque<String>,
    map: HashMap<String, QuantizedVec>,
}

impl QueryLru {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }
    pub fn get(&mut self, key: &str) -> Option<&QuantizedVec> {
        if self.map.contains_key(key) {
            self.order.retain(|k| k != key);
            self.order.push_back(key.to_owned());
        }
        self.map.get(key)
    }
    /// Forgets every entry (memory pressure).
    pub fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }
    pub fn insert(&mut self, key: String, val: QuantizedVec) {
        if self.cap == 0 {
            return;
        }
        self.order.retain(|k| k != &key);
        if !self.map.contains_key(&key) && self.map.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, val);
    }
}

/// Request an embedding only from a caller-selected loopback llama-server port.
/// No server is started and no network address can be configured here.
pub fn local_embedding(port: u16, text: &str) -> Result<QuantizedVec, String> {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let body = serde_json::json!({"content": text}).to_string();
    write!(stream, "POST /embedding HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream
        .take(1024 * 1024)
        .read_to_end(&mut response)
        .map_err(|e| e.to_string())?;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("invalid response")?;
    let header = std::str::from_utf8(&response[..split]).map_err(|e| e.to_string())?;
    if !header
        .lines()
        .next()
        .is_some_and(|s| s.starts_with("HTTP/1.1 200 ") || s.starts_with("HTTP/1.0 200 "))
    {
        return Err("embedding server error".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&response[split + 4..]).map_err(|e| e.to_string())?;
    let embedding = value
        .get("embedding")
        .or_else(|| value.get(0).and_then(|v| v.get("embedding")))
        .ok_or("missing embedding")?;
    let embedding = if embedding.get(0).is_some_and(|v| v.is_array()) {
        &embedding[0]
    } else {
        embedding
    };
    let values = embedding.as_array().ok_or("invalid embedding")?;
    if values.is_empty() || values.len() > 65536 {
        return Err("invalid embedding size".into());
    }
    let values: Vec<f32> = values
        .iter()
        .map(|v| {
            v.as_f64()
                .filter(|x| x.is_finite() && x.abs() <= f32::MAX as f64)
                .map(|x| x as f32)
                .ok_or("invalid embedding value".to_string())
        })
        .collect::<Result<_, _>>()?;
    Ok(QuantizedVec::from_f32(&apply_mrl(values, mrl_dim_from_env()?)?))
}

/// Matryoshka sizes EmbeddingGemma supports; anything else is rejected rather than guessed.
pub const MRL_DIMS: [usize; 4] = [128, 256, 512, 768];

/// Optional Matryoshka truncation from `MEMB_EMBED_MRL_DIM` (until config.toml exists).
/// Unset or empty means full dimension; an unsupported value is an error.
pub fn mrl_dim_from_env() -> Result<Option<usize>, String> {
    match std::env::var("MEMB_EMBED_MRL_DIM") {
        Ok(v) if v.trim().is_empty() => Ok(None),
        Ok(v) => parse_mrl_dim(v.trim()).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err("MEMB_EMBED_MRL_DIM is not valid text".into()),
    }
}

pub fn parse_mrl_dim(text: &str) -> Result<usize, String> {
    let dim: usize = text.parse().map_err(|_| "MRL dimension must be an integer")?;
    if MRL_DIMS.contains(&dim) { Ok(dim) } else { Err(format!("MRL dimension must be one of {MRL_DIMS:?}")) }
}

/// Keep the first `dim` components and L2-renormalize (Matryoshka truncation).
/// `None` returns the vector unchanged. Fails closed on short, non-finite or zero vectors.
pub fn apply_mrl(mut v: Vec<f32>, dim: Option<usize>) -> Result<Vec<f32>, String> {
    let Some(dim) = dim else { return Ok(v) };
    if v.len() < dim { return Err(format!("embedding has {} values, fewer than MRL dimension {dim}", v.len())); }
    v.truncate(dim);
    let norm = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    if !norm.is_finite() || norm < 1e-8 { return Err("embedding cannot be renormalized".into()); }
    Ok(v.into_iter().map(|x| (x as f64 / norm) as f32).collect())
}

/// Cache identity that changes whenever the truncation size does, so stale vectors are never mixed.
pub fn mrl_tag(tag: &str, dim: Option<usize>) -> String {
    match dim { Some(d) => format!("{tag}+mrl{d}"), None => tag.to_owned() }
}

pub fn reciprocal_rank_fusion(lexical_ids: &[u64], vector_ids: &[u64], top_k: usize) -> Vec<u64> {
    const K: f64 = 60.0;
    let mut scores: HashMap<u64, f64> = HashMap::new();
    for (rank, &id) in lexical_ids.iter().enumerate() {
        *scores.entry(id).or_default() += 1.0 / (K + (rank as f64) + 1.0);
    }
    for (rank, &id) in vector_ids.iter().enumerate() {
        *scores.entry(id).or_default() += 1.0 / (K + (rank as f64) + 1.0);
    }
    let mut ranked: Vec<(u64, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(top_k).map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mrl_truncates_to_prefix_and_renormalizes() {
        let v = apply_mrl(vec![3.0, 4.0, 100.0, -100.0], Some(2)).unwrap();
        assert_eq!(v.len(), 2);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert_eq!(apply_mrl(vec![1.0, 2.0], None).unwrap(), vec![1.0, 2.0]);
    }
    #[test]
    fn mrl_fails_closed() {
        assert!(apply_mrl(vec![1.0; 4], Some(8)).is_err()); // shorter than requested
        assert!(apply_mrl(vec![0.0; 8], Some(4)).is_err()); // zero after truncation
        assert!(apply_mrl(vec![f32::NAN, 1.0], Some(2)).is_err());
        assert!(apply_mrl(vec![1e30, 1e30], Some(2)).is_ok()); // f64 norm does not overflow
    }
    #[test]
    fn mrl_dimension_must_be_supported() {
        for ok in ["128", "256", "512", "768"] {
            assert!(parse_mrl_dim(ok).is_ok());
        }
        for bad in ["0", "64", "1024", "-256", "256.0", "abc", ""] {
            assert!(parse_mrl_dim(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn mrl_changes_cache_identity() {
        assert_eq!(mrl_tag("m", None), "m");
        assert_ne!(mrl_tag("m", Some(256)), mrl_tag("m", Some(128)));
        assert_ne!(mrl_tag("m", Some(256)), "m");
    }
    fn lcg_vec(seed: u64, dim: usize) -> Vec<f32> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..dim)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect()
    }

    #[test]
    fn document_keys_are_stable_and_depend_on_every_field() {
        let k = document_key("model|m", "title", "desc");
        // A fixed value: the key must not change between runs, machines or Rust releases.
        let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, document_key("model|m", "title", "desc").iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert_eq!(hex.len(), 64);
        assert_ne!(k, document_key("model|n", "title", "desc"));
        assert_ne!(k, document_key("model|m", "title2", "desc"));
        assert_ne!(k, document_key("model|m", "title", "desc2"));
        // Field boundaries are part of the key.
        assert_ne!(document_key("m", "ab", "c"), document_key("m", "a", "bc"));
        assert_eq!(
            hex,
            "5649b82d0d877364901bbc76005a14d1baa8a5ce86eb203ca51dfd686f093e56",
            "the key format changed: every cached vector would be re-embedded"
        );
    }

    #[test]
    fn prompt_formats_match_the_embeddinggemma_retrieval_format_on_every_path() {
        assert_eq!(gemma::QUERY_PREFIX, "task: search result | query: ");
        assert_eq!(
            format!("{}{}", gemma::DOC_PREFIX, document_text("Redb", "an embedded store")),
            "title: Redb | text: an embedded store"
        );
        assert_eq!(server_document_text("Redb", "an embedded store"), "title: Redb | text: an embedded store");
        assert_eq!(server_query_text("find redb"), "task: search result | query: find redb");
        // A long query keeps its head and tail within the same budget as the in-process model.
        let long: String = (0..6000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let sent = server_query_text(&long);
        let body = sent.strip_prefix(gemma::QUERY_PREFIX).unwrap();
        assert_eq!(body.chars().count(), SERVER_QUERY_MAX_CHARS);
        assert!(body.starts_with(&long[..100]) && body.ends_with(&long[long.len() - 100..]));
        // Multi-byte characters are never split.
        let wide: String = "\u{00e9}".repeat(5000);
        assert_eq!(server_query_text(&wide).strip_prefix(gemma::QUERY_PREFIX).unwrap().chars().count(), SERVER_QUERY_MAX_CHARS);
    }

    #[test]
    fn document_cache_and_hot_set_round_trip_and_reject_damage() {
        let path = std::env::temp_dir().join(format!("memb-embed-hot-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).unwrap();
        let v = QuantizedVec::from_f32(&lcg_vec(1, 64));
        let key = document_key("m", "t", "d");
        assert!(get_document_vectors(&db, &[key]).unwrap().is_empty());
        put_document_vectors(&db, &[(key, v.clone())]).unwrap();
        let back = get_document_vectors(&db, &[key, document_key("m", "x", "y")]).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!((back[&key].scale, &back[&key].data), (v.scale, &v.data));
        // Hot set: absent, then present at its version, byte-exact.
        assert!(read_hot_set(&db, "m").unwrap().is_none());
        let entries: Vec<(u64, QuantizedVec)> = (1..=5).map(|i| (i, QuantizedVec::from_f32(&lcg_vec(i, 64)))).collect();
        write_hot_set(&db, "m", 42, &entries).unwrap();
        let (version, got) = read_hot_set(&db, "m").unwrap().unwrap();
        assert_eq!(version, 42);
        assert_eq!(got.len(), 5);
        for ((ia, va), (ib, vb)) in entries.iter().zip(&got) {
            assert_eq!((ia, va.scale, &va.data), (ib, vb.scale, &vb.data));
        }
        assert!(read_hot_set(&db, "other").unwrap().is_none(), "one row per model tag");
        assert!(write_hot_set(&db, "m", 1, &[(1, QuantizedVec::from_f32(&[1.0, 0.0])), (2, QuantizedVec::from_f32(&[1.0]))]).is_err());
        // A damaged row is treated as absent, never as data.
        {
            let w = db.begin_write().unwrap();
            {
                let mut t = w.open_table(HOT_SET).unwrap();
                t.insert("m", [0u8; 20].as_slice()).unwrap();
                t.insert("short", [0u8; 3].as_slice()).unwrap();
            }
            w.commit().unwrap();
        }
        assert!(read_hot_set(&db, "m").unwrap().is_none());
        assert!(read_hot_set(&db, "short").unwrap().is_none());
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn i8_ranking_agrees_with_f32_on_clustered_vectors() {
        let dim = 768;
        let cosine = |a: &[f32], b: &[f32]| {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            dot / (a.iter().map(|x| x * x).sum::<f32>().sqrt() * b.iter().map(|x| x * x).sum::<f32>().sqrt())
        };
        // 40 topics of 25 notes each: a shared direction plus individual noise.
        let centers: Vec<Vec<f32>> = (0..40).map(|c| lcg_vec(1000 + c, dim)).collect();
        let docs: Vec<Vec<f32>> = (0..1000u64)
            .map(|i| {
                let noise = lcg_vec(i, dim);
                centers[(i % 40) as usize].iter().zip(&noise).map(|(c, n)| c + 0.8 * n).collect()
            })
            .collect();
        let quantized: Vec<QuantizedVec> = docs.iter().map(|d| QuantizedVec::from_f32(d)).collect();
        let mut overlap = 0usize;
        let queries = 40;
        for q in 0..queries {
            let query: Vec<f32> = centers[q].iter().zip(lcg_vec(5000 + q as u64, dim)).map(|(c, n)| c + 0.5 * n).collect();
            let qq = QuantizedVec::from_f32(&query);
            let mut exact: Vec<(f32, usize)> = docs.iter().enumerate().map(|(i, d)| (cosine(&query, d), i)).collect();
            let mut approx: Vec<(f32, usize)> = quantized.iter().enumerate().map(|(i, d)| (qq.cosine(d), i)).collect();
            exact.sort_by(|a, b| b.0.total_cmp(&a.0));
            approx.sort_by(|a, b| b.0.total_cmp(&a.0));
            let top: std::collections::HashSet<usize> = exact.iter().take(10).map(|x| x.1).collect();
            overlap += approx.iter().take(10).filter(|x| top.contains(&x.1)).count();
        }
        let agreement = overlap as f64 / (queries * 10) as f64;
        eprintln!("i8 vs f32 top-10 agreement over {queries} queries: {agreement:.3}");
        assert!(agreement >= 0.95, "agreement {agreement}");
    }

    #[test]
    fn quantization_and_fusion_are_deterministic() {
        let a = QuantizedVec::from_f32(&[1.0, 0.0]);
        let b = QuantizedVec::from_f32(&[0.0, 1.0]);
        assert!(a.cosine(&a) > 0.99);
        assert_eq!(a.cosine(&b), 0.0);
        assert_eq!(reciprocal_rank_fusion(&[1, 2], &[2, 3], 3), vec![2, 1, 3]);
    }
    #[test]
    fn reads_synthetic_loopback_embedding() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            while !request.ends_with(b"\"synthetic\"}") {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            assert!(String::from_utf8_lossy(&request).starts_with("POST /embedding "));
            let body = r#"[{"embedding":[[1.0,0.0]]}]"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let v = local_embedding(port, "synthetic").unwrap();
        assert!(v.cosine(&QuantizedVec::from_f32(&[1.0, 0.0])) > 0.99);
        server.join().unwrap();
    }
    #[test]
    fn lru_tracks_reads_and_zero_capacity() {
        let mut cache = QueryLru::new(2);
        let v = QuantizedVec::from_f32(&[1.0]);
        cache.insert("a".into(), v.clone());
        cache.insert("b".into(), v.clone());
        assert!(cache.get("a").is_some());
        cache.insert("c".into(), v.clone());
        assert!(cache.get("b").is_none());
        cache.insert("a".into(), v.clone());
        cache.insert("d".into(), v.clone());
        assert!(cache.get("a").is_some());
        let mut empty = QueryLru::new(0);
        empty.insert("x".into(), v);
        assert!(empty.get("x").is_none());
    }
}
