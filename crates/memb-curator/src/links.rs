//! Layer 2 bookkeeping that never calls a model: persisted links between staged chunks
//! and between chunks and notes (from the summary vectors), the cache of evidence
//! packets the model has already judged, and the time of the last curation run.
//!
//! All functions take the open database and are meant to run inside `Store::with_db`.
use crate::{CHUNK_TO_CHUNK_THRESHOLD, CHUNK_TO_NODE_THRESHOLD};
use memb_embed::QuantizedVec;
use memb_scroll::ScrollChunk;
use redb::{Database, ReadableTable, TableDefinition};
use std::collections::{BTreeMap, BTreeSet};

pub const LINK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("curator_links_v1");
pub const HASH_CACHE_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("curation_hash_cache_v1");
pub const STATE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("curation_state_v1");

/// Links read back from the table for a set of chunks.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LinkSet {
    /// chunk id -> other chunk ids it is linked to (symmetric).
    pub chunk_links: BTreeMap<String, BTreeSet<String>>,
    /// chunk id -> (note id, cosine) it is linked to.
    pub node_links: BTreeMap<String, Vec<(u64, f32)>>,
}

fn chunk_key(a: &str, b: &str) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    format!("c|{lo}|{hi}")
}

fn node_key(chunk: &str, note: u64) -> String {
    format!("n|{chunk}|{note:020}")
}

fn vector(chunk: &ScrollChunk, model: &str) -> Option<QuantizedVec> {
    let v = chunk.summary_vec.as_ref()?;
    (v.model == model && !v.data.is_empty()).then(|| QuantizedVec {
        scale: v.scale,
        data: v.data.clone(),
    })
}

/// Recomputes and stores the links of `staged` chunks that have a vector tagged `model`:
/// chunk to chunk at cosine >= 0.72 (both chunks staged, same model) and chunk to note at
/// >= 0.70 (`nodes` are the active knowledge and claim notes, embedded with the same
/// model). Earlier links of these chunks are replaced, so a link never outlives its
/// evidence. Returns (chunk links, node links) now stored.
pub fn refresh_links(
    db: &Database,
    staged: &[ScrollChunk],
    nodes: &[(u64, QuantizedVec)],
    model: &str,
) -> Result<(usize, usize), String> {
    let ids: BTreeSet<&str> = staged.iter().map(|c| c.chunk_id.as_str()).collect();
    let vectors: Vec<(&ScrollChunk, QuantizedVec)> = staged
        .iter()
        .filter_map(|c| vector(c, model).map(|v| (c, v)))
        .collect();
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let (mut chunk_links, mut node_links) = (0, 0);
    {
        let mut table = w.open_table(LINK_TABLE).map_err(|e| e.to_string())?;
        let stale: Vec<String> = table
            .iter()
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok().map(|(k, _)| k.value().to_string()))
            .filter(|k| k.split('|').skip(1).take(2).any(|part| ids.contains(part)))
            .collect();
        for key in stale {
            table.remove(key.as_str()).map_err(|e| e.to_string())?;
        }
        for (i, (a, av)) in vectors.iter().enumerate() {
            for (b, bv) in vectors.iter().skip(i + 1) {
                let score = av.cosine(bv);
                if score >= CHUNK_TO_CHUNK_THRESHOLD {
                    let key = chunk_key(&a.chunk_id, &b.chunk_id);
                    table
                        .insert(key.as_str(), score.to_le_bytes().as_slice())
                        .map_err(|e| e.to_string())?;
                    chunk_links += 1;
                }
            }
            for (note, nv) in nodes {
                let score = av.cosine(nv);
                if score >= CHUNK_TO_NODE_THRESHOLD {
                    let key = node_key(&a.chunk_id, *note);
                    table
                        .insert(key.as_str(), score.to_le_bytes().as_slice())
                        .map_err(|e| e.to_string())?;
                    node_links += 1;
                }
            }
        }
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok((chunk_links, node_links))
}

/// Reads the stored links that involve any of `chunk_ids`.
pub fn read_links(db: &Database, chunk_ids: &[String]) -> Result<LinkSet, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(LINK_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(LinkSet::default()),
        Err(e) => return Err(e.to_string()),
    };
    let wanted: BTreeSet<&str> = chunk_ids.iter().map(String::as_str).collect();
    let mut set = LinkSet::default();
    for entry in table.iter().map_err(|e| e.to_string())? {
        let (key, value) = entry.map_err(|e| e.to_string())?;
        let parts: Vec<&str> = key.value().split('|').collect();
        let score = value
            .value()
            .try_into()
            .map(f32::from_le_bytes)
            .map_err(|_| "stored link is unreadable".to_string())?;
        match parts.as_slice() {
            ["c", a, b] if wanted.contains(a) && wanted.contains(b) => {
                set.chunk_links.entry((*a).into()).or_default().insert((*b).into());
                set.chunk_links.entry((*b).into()).or_default().insert((*a).into());
            }
            ["n", chunk, note] if wanted.contains(chunk) => {
                if let Ok(note) = note.parse::<u64>() {
                    set.node_links.entry((*chunk).into()).or_default().push((note, score));
                }
            }
            _ => {}
        }
    }
    Ok(set)
}

/// True if an evidence packet with this hash was already sent to the model within
/// `ttl_ms` (so it is not sent again until its content changes or the entry expires).
pub fn hash_seen(db: &Database, hash: &str, now_ms: u64, ttl_ms: u64) -> Result<bool, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(HASH_CACHE_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let found = table.get(hash).map_err(|e| e.to_string())?;
    Ok(found
        .and_then(|v| <[u8; 8]>::try_from(v.value()).ok())
        .map(u64::from_le_bytes)
        .is_some_and(|at| now_ms.saturating_sub(at) <= ttl_ms))
}

/// Records that these packets were judged by the model at `now_ms`, and drops entries
/// older than `ttl_ms`.
pub fn record_hashes(db: &Database, hashes: &[String], now_ms: u64, ttl_ms: u64) -> Result<(), String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = w.open_table(HASH_CACHE_TABLE).map_err(|e| e.to_string())?;
        let old: Vec<String> = table
            .iter()
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .filter(|(_, v)| {
                <[u8; 8]>::try_from(v.value())
                    .map(u64::from_le_bytes)
                    .map_or(true, |at| now_ms.saturating_sub(at) > ttl_ms)
            })
            .map(|(k, _)| k.value().to_string())
            .collect();
        for key in old {
            table.remove(key.as_str()).map_err(|e| e.to_string())?;
        }
        for hash in hashes {
            table
                .insert(hash.as_str(), now_ms.to_le_bytes().as_slice())
                .map_err(|e| e.to_string())?;
        }
    }
    w.commit().map_err(|e| e.to_string())
}

/// When a curation run last finished (0 if never).
pub fn last_curation_ms(db: &Database) -> Result<u64, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(STATE_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.to_string()),
    };
    let found = table.get("last_curation_ms").map_err(|e| e.to_string())?;
    Ok(found
        .and_then(|v| <[u8; 8]>::try_from(v.value()).ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0))
}

pub fn set_last_curation_ms(db: &Database, now_ms: u64) -> Result<(), String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = w.open_table(STATE_TABLE).map_err(|e| e.to_string())?;
        table
            .insert("last_curation_ms", now_ms.to_le_bytes().as_slice())
            .map_err(|e| e.to_string())?;
    }
    w.commit().map_err(|e| e.to_string())
}

/// The quota backoff of model number `slot` after it said its usage quota was used up:
/// (wait until this UNIX time in ms, consecutive hits). (0, 0) when there is none.
pub fn quota_backoff(db: &Database, slot: usize) -> Result<(u64, u32), String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(STATE_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((0, 0)),
        Err(e) => return Err(e.to_string()),
    };
    let found = table.get(format!("quota_backoff_{slot}").as_str()).map_err(|e| e.to_string())?;
    Ok(found
        .and_then(|v| {
            let b = v.value();
            let until = u64::from_le_bytes(<[u8; 8]>::try_from(b.get(..8)?).ok()?);
            let strikes = u32::from_le_bytes(<[u8; 4]>::try_from(b.get(8..12)?).ok()?);
            Some((until, strikes))
        })
        .unwrap_or((0, 0)))
}

pub fn set_quota_backoff(db: &Database, slot: usize, until_ms: u64, strikes: u32) -> Result<(), String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = w.open_table(STATE_TABLE).map_err(|e| e.to_string())?;
        let mut v = until_ms.to_le_bytes().to_vec();
        v.extend_from_slice(&strikes.to_le_bytes());
        table
            .insert(format!("quota_backoff_{slot}").as_str(), v.as_slice())
            .map_err(|e| e.to_string())?;
    }
    w.commit().map_err(|e| e.to_string())
}

/// The kill switch: while set, nothing is applied automatically (proposals stay pending).
pub fn auto_apply_paused(db: &Database) -> Result<bool, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(STATE_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let found = table.get("auto_apply_paused").map_err(|e| e.to_string())?;
    Ok(found.is_some_and(|v| v.value() == b"1"))
}

pub fn set_auto_apply_paused(db: &Database, paused: bool) -> Result<(), String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = w.open_table(STATE_TABLE).map_err(|e| e.to_string())?;
        table
            .insert("auto_apply_paused", if paused { b"1".as_slice() } else { b"0".as_slice() })
            .map_err(|e| e.to_string())?;
    }
    w.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use memb_scroll::ChunkVec;

    fn chunk(id: &str, v: &[f32], model: &str) -> ScrollChunk {
        let q = QuantizedVec::from_f32(v);
        ScrollChunk {
            chunk_id: id.into(),
            session_id: id.split('_').nth(1).unwrap_or("s").into(),
            chunk_index: 0,
            messages: vec![],
            summary: Some("- s".into()),
            created_at_ms: 1,
            curated: false,
            explicit_signal: None,
            summary_vec: Some(ChunkVec { model: model.into(), scale: q.scale, data: q.data }),
        }
    }

    fn db() -> (Database, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "memb_links_{}_{}.redb",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        (Database::create(&p).unwrap(), p)
    }

    #[test]
    fn links_follow_the_thresholds_the_model_tag_and_are_replaced_on_refresh() {
        let (db, path) = db();
        let a = chunk("chk_s1_0000", &[1.0, 0.0, 0.0], "m");
        let b = chunk("chk_s2_0000", &[0.9, 0.1, 0.0], "m"); // close to a
        let c = chunk("chk_s3_0000", &[0.0, 1.0, 0.0], "m"); // orthogonal
        let d = chunk("chk_s4_0000", &[1.0, 0.0, 0.0], "other-model"); // same vector, other model
        let nodes = vec![(7u64, QuantizedVec::from_f32(&[0.95, 0.05, 0.0])), (8u64, QuantizedVec::from_f32(&[0.0, 0.0, 1.0]))];
        let staged = vec![a.clone(), b.clone(), c.clone(), d.clone()];
        let (cc, nn) = refresh_links(&db, &staged, &nodes, "m").unwrap();
        assert_eq!((cc, nn), (1, 2), "a-b are linked; a and b touch note 7 only");
        let ids: Vec<String> = staged.iter().map(|c| c.chunk_id.clone()).collect();
        let links = read_links(&db, &ids).unwrap();
        assert!(links.chunk_links["chk_s1_0000"].contains("chk_s2_0000"));
        assert!(links.chunk_links["chk_s2_0000"].contains("chk_s1_0000"));
        assert!(!links.chunk_links.contains_key("chk_s3_0000"));
        assert!(!links.chunk_links.contains_key("chk_s4_0000"), "a vector from another model is never compared");
        assert_eq!(links.node_links["chk_s1_0000"].iter().map(|n| n.0).collect::<Vec<_>>(), vec![7]);
        assert!(!links.node_links.contains_key("chk_s3_0000"));
        // Refresh with the nodes gone: the old node links disappear, the chunk link stays.
        let (cc, nn) = refresh_links(&db, &staged, &[], "m").unwrap();
        assert_eq!((cc, nn), (1, 0));
        assert!(read_links(&db, &ids).unwrap().node_links.is_empty());
        // Refresh without b: a's link to b is dropped.
        let only_a = vec![a.clone(), c.clone()];
        refresh_links(&db, &only_a, &[], "m").unwrap();
        assert!(read_links(&db, &ids).unwrap().chunk_links.is_empty());
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn judged_packets_are_remembered_until_the_limit_and_the_clock_is_stored() {
        let (db, path) = db();
        let day = 24 * 3600 * 1000;
        assert!(!hash_seen(&db, "h1", 10, day).unwrap());
        record_hashes(&db, &["h1".into(), "h2".into()], 1_000, day).unwrap();
        assert!(hash_seen(&db, "h1", 1_000 + day, day).unwrap());
        assert!(!hash_seen(&db, "h1", 1_001 + day, day).unwrap(), "an old judgement expires");
        assert!(!hash_seen(&db, "h3", 1_000, day).unwrap());
        record_hashes(&db, &["h3".into()], 2 * day + 5_000, day).unwrap();
        assert!(!hash_seen(&db, "h2", 2 * day + 5_000, day).unwrap(), "expired entries are dropped");
        assert_eq!(last_curation_ms(&db).unwrap(), 0);
        set_last_curation_ms(&db, 42).unwrap();
        assert_eq!(last_curation_ms(&db).unwrap(), 42);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
