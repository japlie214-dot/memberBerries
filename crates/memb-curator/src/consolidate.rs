//! Phase 4 graph consolidation: finds near-duplicate knowledge or claim notes and plans a
//! reversible merge. Pure: it reads a snapshot and changes nothing. The merge is two
//! ordinary curator proposals (an update of the survivor and a retraction of the duplicate),
//! so it waits for the user's approval like any other proposal and can be rolled back
//! with `memb proposals rollback`.
//!
//! No model rewrites any text: the survivor keeps its own title and description, gains the
//! duplicate's parents, and records the duplicate's id under `merged_from`. Only notes that
//! carry curator provenance (`source_chunks`) are considered, because a proposal must cite
//! its sources, and notes the user wrote by hand are never touched.
use crate::CuratorAction;
use memb_core::{Kind, Note, NoteStatus, NotePatch};
use memb_embed::QuantizedVec;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Cosine similarity above which two active notes of the same kind count as duplicates.
pub const MERGE_COSINE: f32 = 0.88;

#[derive(Debug, Clone)]
pub struct MergePlan {
    pub survivor: u64,
    pub duplicate: u64,
    pub cosine: f32,
    pub sources: Vec<(String, String)>,
    /// Stable for this pair at these versions of the two notes, so a pair that was
    /// already proposed (and refused or ignored) is not proposed again until one changes.
    pub hash: String,
    update: CuratorAction,
}

impl MergePlan {
    /// The two proposals: update the survivor, then retract the duplicate.
    pub fn actions(&self) -> (CuratorAction, CuratorAction) {
        (
            self.update.clone(),
            CuratorAction::Retract { id: self.duplicate, migrate_to_topic_id: None, force_cascade: false },
        )
    }
}

impl MergePlan {
    /// The two actions as stored in the proposal ledger (the same JSON a model reply
    /// would carry), and nothing else about them is trusted.
    pub fn action_json(&self) -> (Value, Value) {
        let CuratorAction::Update { id, patch } = &self.update else { unreachable!("a merge plan always updates") };
        let mut body = serde_json::Map::new();
        if let Some(parents) = &patch.parents {
            body.insert("parents".into(), json!(parents));
        }
        if let Some(properties) = &patch.properties {
            body.insert("properties".into(), json!(properties));
        }
        (
            json!({"action": "UPDATE", "id": id, "patch": body}),
            json!({"action": "RETRACT", "id": self.duplicate, "migrate_to_topic_id": null, "force_cascade": false}),
        )
    }
}

fn sources_of(note: &Note) -> Vec<(String, String)> {
    note.properties
        .get("source_chunks")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let session = item.get("session_id")?.as_str()?.trim();
                    let chunk = item.get("chunk_id")?.as_str()?.trim();
                    (!session.is_empty() && !chunk.is_empty()).then(|| (session.to_owned(), chunk.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The most notes compared in one run (pairs grow with the square): the newest ones, since
/// vectors arrive in note-id order. 2000 notes is about two million comparisons.
pub const MAX_SCAN: usize = 2000;

/// Up to `max` merge plans, most similar first. A note appears in at most one plan.
pub fn find_merges(notes: &BTreeMap<u64, Note>, vectors: &[(u64, QuantizedVec)], max: usize) -> Vec<MergePlan> {
    let vectors = &vectors[vectors.len().saturating_sub(MAX_SCAN)..];
    let has_active_children = |id: u64| notes.values().any(|n| n.status == NoteStatus::Active && n.parents.contains(&id));
    let mut pairs: Vec<(f32, u64, u64)> = Vec::new();
    for (i, (a_id, a_vec)) in vectors.iter().enumerate() {
        for (b_id, b_vec) in &vectors[i + 1..] {
            let cosine = a_vec.cosine(b_vec);
            if cosine > MERGE_COSINE {
                pairs.push((cosine, *a_id.min(b_id), *a_id.max(b_id)));
            }
        }
    }
    pairs.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
    let mut used = BTreeSet::new();
    let mut plans = Vec::new();
    for (cosine, lo, hi) in pairs {
        if plans.len() >= max {
            break;
        }
        let (Some(a), Some(b)) = (notes.get(&lo), notes.get(&hi)) else { continue };
        let mergeable = |n: &Note| n.status == NoteStatus::Active && matches!(n.kind, Kind::Knowledge | Kind::Claim);
        if !mergeable(a) || !mergeable(b) || a.kind != b.kind || used.contains(&lo) || used.contains(&hi) {
            continue;
        }
        let (sa, sb) = (sources_of(a), sources_of(b));
        if sa.is_empty() || sb.is_empty() {
            continue; // not curator-made: never touched
        }
        // The survivor must keep every child edge: the duplicate may have none. If only one
        // of the two has children it survives; if neither has, the older one does.
        let (a_kids, b_kids) = (has_active_children(lo), has_active_children(hi));
        let (survivor, duplicate) = match (a_kids, b_kids) {
            (true, true) => continue,
            (false, true) => (b, a),
            _ => (a, b),
        };
        let parents: BTreeSet<u64> = survivor.parents.iter().chain(&duplicate.parents).copied().collect();
        let parents: Vec<u64> = parents.into_iter().collect();
        let mut merged_from: Vec<u64> = survivor
            .properties
            .get("merged_from")
            .and_then(Value::as_array)
            .map(|v| v.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        merged_from.push(duplicate.id);
        merged_from.sort_unstable();
        merged_from.dedup();
        let mut properties = BTreeMap::new();
        properties.insert("merged_from".to_owned(), json!(merged_from));
        let patch = NotePatch {
            parents: (parents != survivor.parents).then_some(parents),
            properties: Some(properties),
            ..Default::default()
        };
        let sources: Vec<(String, String)> = sa.into_iter().chain(sb).collect::<BTreeSet<_>>().into_iter().collect();
        let hash = format!(
            "{:x}",
            Sha256::digest(format!("merge:{}:{}:{}:{}", survivor.id, duplicate.id, survivor.updated_at_ms, duplicate.updated_at_ms))
        );
        used.insert(lo);
        used.insert(hi);
        plans.push(MergePlan {
            survivor: survivor.id,
            duplicate: duplicate.id,
            cosine,
            sources,
            hash,
            update: CuratorAction::Update { id: survivor.id, patch },
        });
    }
    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: u64, kind: Kind, parents: Vec<u64>, with_sources: bool) -> Note {
        let mut properties = BTreeMap::new();
        if with_sources {
            properties.insert("source_chunks".to_owned(), json!([{"session_id": "s", "chunk_id": format!("c{id}")}]));
        }
        Note { id, title: format!("n{id}"), desc: "d".into(), kind, status: NoteStatus::Active, parents, properties, supersedes: None, created_at_ms: 1, updated_at_ms: 10 + id }
    }

    fn unit(x: f32, y: f32) -> QuantizedVec {
        let n = (x * x + y * y).sqrt();
        QuantizedVec::from_f32(&[x / n, y / n])
    }

    #[test]
    fn near_duplicates_of_the_same_kind_with_provenance_become_a_merge_plan() {
        let notes: BTreeMap<u64, Note> = [
            note(1, Kind::Topic, vec![], false),
            note(2, Kind::Knowledge, vec![1], true),
            note(3, Kind::Knowledge, vec![1], true),
            note(4, Kind::Knowledge, vec![1], true), // different content
            note(5, Kind::Claim, vec![2], true),     // a claim: other kind, even if the vector matches
        ]
        .into_iter()
        .map(|n| (n.id, n))
        .collect();
        let vectors = vec![(2, unit(1.0, 0.0)), (3, unit(0.99, 0.12)), (4, unit(0.0, 1.0)), (5, unit(1.0, 0.0))];
        let plans = find_merges(&notes, &vectors, 3);
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!((p.survivor, p.duplicate), (2, 3), "the older note survives when neither has children");
        assert!(p.cosine > MERGE_COSINE);
        assert_eq!(p.sources, vec![("s".to_string(), "c2".to_string()), ("s".to_string(), "c3".to_string())]);
        let (update, retract) = p.actions();
        assert!(matches!(retract, CuratorAction::Retract { id: 3, migrate_to_topic_id: None, force_cascade: false }));
        let CuratorAction::Update { id, patch } = update else { panic!("update expected") };
        assert_eq!(id, 2);
        assert_eq!(patch.properties.unwrap()["merged_from"], json!([3]));
        assert_eq!(patch.parents, None, "same parents: nothing to change");
        // The hash is stable for the same versions and changes when a note changes.
        let again = find_merges(&notes, &vectors, 3);
        assert_eq!(again[0].hash, p.hash);
        let mut changed = notes.clone();
        changed.get_mut(&3).unwrap().updated_at_ms += 1;
        assert_ne!(find_merges(&changed, &vectors, 3)[0].hash, p.hash);
    }

    #[test]
    fn hand_written_notes_children_and_dissimilar_pairs_are_never_merged() {
        let mut notes: BTreeMap<u64, Note> = [
            note(1, Kind::Topic, vec![], false),
            note(2, Kind::Knowledge, vec![1], true),
            note(3, Kind::Knowledge, vec![1], false), // no provenance: written by hand
            note(4, Kind::Knowledge, vec![1], true),
            note(5, Kind::Knowledge, vec![1], true),
        ]
        .into_iter()
        .map(|n| (n.id, n))
        .collect();
        let same = vec![(2, unit(1.0, 0.0)), (3, unit(1.0, 0.01))];
        assert!(find_merges(&notes, &same, 3).is_empty(), "a hand-written note is never touched");
        // 4 and 5 are duplicates, and 5 has a claim under it: 5 survives so no edge is lost.
        notes.insert(6, note(6, Kind::Claim, vec![5], true));
        let dup = vec![(4, unit(1.0, 0.0)), (5, unit(1.0, 0.02))];
        let plans = find_merges(&notes, &dup, 3);
        assert_eq!((plans[0].survivor, plans[0].duplicate), (5, 4));
        // Both with children: skipped.
        notes.insert(7, note(7, Kind::Claim, vec![4], true));
        assert!(find_merges(&notes, &dup, 3).is_empty());
        // Below the threshold: skipped.
        let apart = vec![(4, unit(1.0, 0.0)), (5, unit(0.8, 0.6))];
        assert!(find_merges(&notes, &apart, 3).is_empty());
        // The limit caps the number of plans.
        assert!(find_merges(&notes, &dup, 0).is_empty());
        // Only the newest MAX_SCAN notes are compared: a duplicate pair that starts before
        // that window is not seen, one inside it is.
        notes.insert(8, note(8, Kind::Knowledge, vec![1], true));
        notes.insert(9, note(9, Kind::Knowledge, vec![1], true));
        let pair = vec![(8, unit(1.0, 0.0)), (9, unit(1.0, 0.02))];
        let filler = |n: usize| (0..n).map(|i| (1000 + i as u64, unit(0.0, 1.0)));
        let inside: Vec<_> = pair.iter().cloned().chain(filler(MAX_SCAN - 2)).collect();
        assert_eq!(find_merges(&notes, &inside, 3).len(), 1, "the pair is inside the window");
        let outside: Vec<_> = pair.iter().cloned().chain(filler(MAX_SCAN)).collect();
        assert!(find_merges(&notes, &outside, 3).is_empty(), "older than the newest {MAX_SCAN}: not seen");
    }
}
