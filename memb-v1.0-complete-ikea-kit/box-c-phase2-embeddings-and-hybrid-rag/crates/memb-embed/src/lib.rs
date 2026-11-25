use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

pub const QUERY_LRU_CAP: usize = 256;
pub const DOC_EMBED_CACHE: TableDefinition<u64, &[u8]> = TableDefinition::new("doc_embed_cache_v1");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantizedVec {
    pub scale: f32,
    pub data: Vec<i8>,
}

impl QuantizedVec {
    pub fn from_f32(v: &[f32]) -> Self {
        let max_abs = v.iter().map(|x| x.abs()).fold(0.0f32, f32::max).max(1e-8);
        let scale = max_abs / 127.0;
        let data = v.iter().map(|&x| (x / scale).round().clamp(-127.0, 127.0) as i8).collect();
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
        if denom <= 1e-8 { 0.0 } else { (dot as f32) / denom }
    }
}

pub struct QueryLru {
    cap: usize,
    order: VecDeque<String>,
    map: HashMap<String, QuantizedVec>,
}

impl QueryLru {
    pub fn new(cap: usize) -> Self {
        Self { cap, order: VecDeque::new(), map: HashMap::new() }
    }
    pub fn get(&self, key: &str) -> Option<&QuantizedVec> { self.map.get(key) }
    pub fn insert(&mut self, key: String, val: QuantizedVec) {
        if !self.map.contains_key(&key) && self.map.len() >= self.cap {
            if let Some(old) = self.order.pop_front() { self.map.remove(&old); }
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
        tbl.insert(note_id, bytes.as_slice()).map_err(|e| e.to_string())?;
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(())
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
