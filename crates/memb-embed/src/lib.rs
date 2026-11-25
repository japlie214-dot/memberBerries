pub mod gemma;

use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

pub const QUERY_LRU_CAP: usize = 256;
pub const DOC_EMBED_CACHE: TableDefinition<u64, &[u8]> = TableDefinition::new("doc_embed_cache_v1");
const DOC_EMBED_CACHE_V2: TableDefinition<u64, &[u8]> = TableDefinition::new("doc_embed_cache_v2");

#[derive(Serialize, Deserialize)]
struct CachedDocument {
    model: String,
    fingerprint: [u64; 2],
    vector: QuantizedVec,
}

/// Include both title and description in cache invalidation without retaining text.
pub fn document_fingerprint(title: &str, desc: &str) -> [u64; 2] {
    use std::hash::{Hash, Hasher};
    let calculate = |salt: u8| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        salt.hash(&mut h);
        title.hash(&mut h);
        desc.hash(&mut h);
        h.finish()
    };
    [calculate(0), calculate(1)]
}

pub fn read_document_cache(
    db: &Database,
    model: &str,
) -> Result<HashMap<u64, ([u64; 2], QuantizedVec)>, String> {
    use redb::ReadableTable;
    let read = db.begin_read().map_err(|e| e.to_string())?;
    let table = match read.open_table(DOC_EMBED_CACHE_V2) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut entries = HashMap::new();
    for row in table.iter().map_err(|e| e.to_string())? {
        let (id, bytes) = row.map_err(|e| e.to_string())?;
        if let Ok(entry) = serde_json::from_slice::<CachedDocument>(bytes.value()) {
            if entry.model == model {
                entries.insert(id.value(), (entry.fingerprint, entry.vector));
            }
        }
    }
    Ok(entries)
}

pub fn write_document_cache(
    db: &Database,
    model: &str,
    entries: &[(u64, [u64; 2], QuantizedVec)],
) -> Result<(), String> {
    let write = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = write
            .open_table(DOC_EMBED_CACHE_V2)
            .map_err(|e| e.to_string())?;
        for (id, fingerprint, vector) in entries {
            let data = serde_json::to_vec(&CachedDocument {
                model: model.to_owned(),
                fingerprint: *fingerprint,
                vector: vector.clone(),
            })
            .map_err(|e| e.to_string())?;
            table
                .insert(*id, data.as_slice())
                .map_err(|e| e.to_string())?;
        }
    }
    write.commit().map_err(|e| e.to_string())
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

pub fn cache_doc_embedding(db: &Database, note_id: u64, vec: &QuantizedVec) -> Result<(), String> {
    let bytes = serde_json::to_vec(vec).map_err(|e| e.to_string())?;
    let w = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut tbl = w.open_table(DOC_EMBED_CACHE).map_err(|e| e.to_string())?;
        tbl.insert(note_id, bytes.as_slice())
            .map_err(|e| e.to_string())?;
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(())
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
    Ok(QuantizedVec::from_f32(&values))
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
