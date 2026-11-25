use memb_core::{Draft, Kind, NotePatch};
use memb_embed::QuantizedVec;
use memb_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const CHUNK_TO_CHUNK_THRESHOLD: f32 = 0.72;
pub const CHUNK_TO_NODE_THRESHOLD: f32 = 0.70;
pub const STAGING_TTL_DAYS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkVector {
    pub chunk_id: String,
    pub session_id: String,
    pub summary: String,
    pub vec: QuantizedVec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeVector {
    pub note_id: u64,
    pub title: String,
    pub vec: QuantizedVec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePacket {
    pub reason: String,
    pub source_chunks: Vec<(String, String)>, // (session_id, chunk_id)
    pub distinct_sessions: usize,
    pub related_note_ids: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CuratorAction {
    Defer,
    KnowledgeUpsert { title: String, desc: String, parents: Vec<u64>, #[serde(default)] properties: BTreeMap<String, Value> },
    ClaimAdd { title: String, desc: String, parents: Vec<u64>, #[serde(default)] properties: BTreeMap<String, Value> },
    ClaimSupersede { old_id: u64, title: String, desc: String, parents: Vec<u64>, #[serde(default)] properties: BTreeMap<String, Value> },
    Retract { id: u64, migrate_to_topic_id: Option<u64>, #[serde(default)] force_cascade: bool },
    Update { id: u64, patch: NotePatch },
}

pub fn build_evidence_packets(chunks: &[ChunkVector], nodes: &[NodeVector]) -> Vec<EvidencePacket> {
    let mut packets = Vec::new();
    let mut used = vec![false; chunks.len()];

    for i in 0..chunks.len() {
        if used[i] { continue; }
        let mut cluster = vec![i];
        for j in (i + 1)..chunks.len() {
            if !used[j] && chunks[i].vec.cosine(&chunks[j].vec) >= CHUNK_TO_CHUNK_THRESHOLD {
                cluster.push(j);
            }
        }

        let mut related_nodes = Vec::new();
        for &idx in &cluster {
            for nv in nodes {
                if chunks[idx].vec.cosine(&nv.vec) >= CHUNK_TO_NODE_THRESHOLD && !related_nodes.contains(&nv.note_id) {
                    related_nodes.push(nv.note_id);
                }
            }
        }

        let lower = chunks[i].summary.to_lowercase();
        let explicit_signal = lower.contains("always use")
            || lower.contains("never use")
            || lower.contains("remember that")
            || lower.contains("from now on");

        if cluster.len() >= 2 || !related_nodes.is_empty() || explicit_signal {
            let mut sessions = BTreeSet::new();
            let mut source_chunks = Vec::new();
            for &idx in &cluster {
                used[idx] = true;
                sessions.insert(chunks[idx].session_id.clone());
                source_chunks.push((chunks[idx].session_id.clone(), chunks[idx].chunk_id.clone()));
            }
            let reason = if cluster.len() >= 2 {
                "corroboration_cluster"
            } else if !related_nodes.is_empty() {
                "existing_node_touch"
            } else {
                "explicit_user_directive"
            };
            packets.push(EvidencePacket {
                reason: reason.into(),
                source_chunks,
                distinct_sessions: sessions.len(),
                related_note_ids: related_nodes,
            });
        }
    }
    packets
}

/// Applies a validated CuratorAction to `Store`, automatically stamping `properties.source_chunks`.
pub fn apply_curator_action(
    store: &Store,
    action: CuratorAction,
    source_chunks: &[(String, String)],
) -> Result<Option<u64>, String> {
    let provenance: Vec<Value> = source_chunks
        .iter()
        .map(|(s, c)| json!({ "session_id": s, "chunk_id": c }))
        .collect();

    match action {
        CuratorAction::Defer => Ok(None),
        CuratorAction::KnowledgeUpsert { title, desc, parents, mut properties } => {
            properties.insert("source_chunks".into(), Value::Array(provenance));
            let n = store.remember(Draft { title, desc, kind: Kind::Knowledge, parents, properties })?;
            Ok(Some(n.id))
        }
        CuratorAction::ClaimAdd { title, desc, parents, mut properties } => {
            properties.insert("source_chunks".into(), Value::Array(provenance));
            let n = store.remember(Draft { title, desc, kind: Kind::Claim, parents, properties })?;
            Ok(Some(n.id))
        }
        CuratorAction::ClaimSupersede { old_id, title, desc, parents, mut properties } => {
            properties.insert("source_chunks".into(), Value::Array(provenance));
            let n = store.supersede(old_id, Draft { title, desc, kind: Kind::Claim, parents, properties })?;
            Ok(Some(n.id))
        }
        CuratorAction::Update { id, patch } => {
            let n = store.update(id, patch)?;
            Ok(Some(n.id))
        }
        CuratorAction::Retract { id, migrate_to_topic_id, force_cascade } => {
            store.retract(id, migrate_to_topic_id, force_cascade)?;
            Ok(Some(id))
        }
    }
}

pub const CURATOR_SYSTEM_PROMPT: &str = r#"You are the memb Layer-2 Memory Curator.
Emit a JSON object with `action` chosen strictly from:
  DEFER | KNOWLEDGE_UPSERT | CLAIM_ADD | CLAIM_SUPERSEDE | UPDATE | RETRACT
Rules:
1. Default to DEFER for one-off noise or synthetic test strings ("secret code is 123").
2. Keep `desc` crisp; put structured metadata in `properties`.
3. Every KNOWLEDGE_UPSERT links >= 1 active `topic`; every CLAIM links >= 1 active `knowledge`.
4. Always run CLI curator backends with MEMB_INTERNAL=1."#;
