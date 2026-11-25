//! Evidence packets built from the stored links, with no model and no vectors computed
//! here. A staged chunk enters a packet only if it meets at least one Phase 3 rule:
//! (1) linked to another staged chunk (cosine >= 0.72), (2) linked to an existing
//! knowledge or claim note (>= 0.70), or (3) an explicit user directive.
use crate::links::LinkSet;
use memb_core::{Kind, Note, NoteStatus};
use memb_scroll::{Role, ScrollChunk};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Most chunks in one packet; a larger linked group is split.
pub const MAX_CHUNKS_PER_PACKET: usize = 5;
/// Most related notes listed in one packet.
pub const MAX_RELATED_NODES: usize = 10;
const DIRECTIVES: [&str; 4] = ["always use", "never use", "remember that", "from now on"];
const MAX_EXCERPT_BYTES: usize = 3000;
const MAX_NODE_DESC_CHARS: usize = 300;

#[derive(Debug, Clone, Serialize)]
pub struct ChunkBrief {
    pub session_id: String,
    pub chunk_id: String,
    /// The model-written summary; `None` if the chunk was never summarized.
    pub summary: Option<String>,
    pub explicit_directive: bool,
    /// Bounded start of the user's messages, only for chunks without a summary.
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeBrief {
    pub id: u64,
    pub kind: String,
    pub title: String,
    pub desc: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkedPacket {
    pub reason: String,
    pub chunks: Vec<ChunkBrief>,
    /// Two or more distinct sessions is a strong durability signal.
    pub distinct_sessions: usize,
    /// File paths, crate names, flags and similar tokens that two or more chunks share.
    pub shared_entities: Vec<String>,
    /// Active knowledge and claim notes the chunks are linked to, best match first.
    pub related_nodes: Vec<NodeBrief>,
    /// Heuristic: a `key=value` or `key: value` pair in the chunk summaries disagrees
    /// with the same key in a related note.
    pub conflict_hint: bool,
    /// Identifies this exact content (chunks, summaries, related notes and their edit
    /// times); an unchanged packet is not sent to the model twice.
    pub hash: String,
}

fn has_directive(text: &str) -> bool {
    let lower = text.to_lowercase();
    DIRECTIVES.iter().any(|d| lower.contains(d))
}

/// Rule 3: the summarizer flagged a standing directive, or one of the user's own
/// messages (or the summary) contains a directive phrase.
pub fn explicit_directive(chunk: &ScrollChunk) -> bool {
    chunk.explicit_signal == Some(true)
        || chunk.summary.as_deref().is_some_and(has_directive)
        || chunk
            .messages
            .iter()
            .any(|m| m.role == Role::User && has_directive(&m.content))
}

fn excerpt(chunk: &ScrollChunk) -> String {
    let mut text = String::new();
    for m in chunk.messages.iter().filter(|m| m.role == Role::User) {
        text.push_str(&m.content);
        text.push('\n');
        if text.len() >= MAX_EXCERPT_BYTES {
            break;
        }
    }
    let mut end = text.len().min(MAX_EXCERPT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// Tokens that look like a file path, a source file, a flag or a Rust path.
fn entities(text: &str) -> BTreeSet<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"' | '\'' | '`' | '[' | ']'))
        .map(|t| t.trim_end_matches(['.', ':']))
        .filter(|t| t.len() >= 3 && t.len() <= 80)
        .filter(|t| {
            t.starts_with("--")
                || t.contains("::")
                || t.contains('/')
                || t.contains('\\')
                || [".rs", ".toml", ".md", ".json", ".exe", ".ps1", ".py"].iter().any(|e| t.ends_with(e))
        })
        .map(str::to_owned)
        .collect()
}

/// `key=value` and `key: value` pairs with short alphanumeric keys and values.
fn pairs(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('-').trim();
        for sep in ['=', ':'] {
            if let Some((k, v)) = line.split_once(sep) {
                let (k, v) = (k.trim().to_lowercase(), v.split_whitespace().next().unwrap_or("").to_lowercase());
                let key_ok = !k.is_empty() && k.len() <= 40 && k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.');
                let val_ok = !v.is_empty() && v.len() <= 40;
                if key_ok && val_ok {
                    map.insert(k, v);
                }
            }
        }
    }
    map
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Builds packets from the links of `chunks` (all staged, uncurated). A chunk that meets
/// none of the three rules is not in any packet and stays staged.
pub fn build_linked_packets(
    chunks: &[ScrollChunk],
    links: &LinkSet,
    notes: &BTreeMap<u64, Note>,
) -> Vec<LinkedPacket> {
    let by_id: BTreeMap<&str, &ScrollChunk> = chunks.iter().map(|c| (c.chunk_id.as_str(), c)).collect();
    // Connected components over chunk-to-chunk links.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut groups: Vec<Vec<&ScrollChunk>> = Vec::new();
    for chunk in chunks {
        if seen.contains(chunk.chunk_id.as_str()) {
            continue;
        }
        let mut component = vec![chunk.chunk_id.as_str()];
        seen.insert(chunk.chunk_id.as_str());
        let mut at = 0;
        while at < component.len() {
            let current = component[at];
            at += 1;
            for next in links.chunk_links.get(current).into_iter().flatten() {
                if by_id.contains_key(next.as_str()) && seen.insert(next.as_str()) {
                    component.push(by_id[next.as_str()].chunk_id.as_str());
                }
            }
        }
        component.sort_unstable();
        groups.push(component.into_iter().map(|id| by_id[id]).collect());
    }
    let mut packets = Vec::new();
    for group in groups {
        // A group larger than the packet limit is split into consecutive packets.
        for part in group.chunks(MAX_CHUNKS_PER_PACKET) {
            let has_peer = part.len() >= 2;
            let mut related: BTreeMap<u64, f32> = BTreeMap::new();
            for c in part {
                for (note, score) in links.node_links.get(&c.chunk_id).into_iter().flatten() {
                    let usable = notes.get(note).is_some_and(|n| {
                        n.status == NoteStatus::Active && matches!(n.kind, Kind::Knowledge | Kind::Claim)
                    });
                    if usable {
                        let best = related.entry(*note).or_insert(0.0);
                        *best = best.max(*score);
                    }
                }
            }
            let directive = part.iter().any(|c| explicit_directive(c));
            if !has_peer && related.is_empty() && !directive {
                continue; // none of the three rules: stays staged
            }
            let mut related: Vec<(u64, f32)> = related.into_iter().collect();
            related.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            related.truncate(MAX_RELATED_NODES);
            let related_nodes: Vec<NodeBrief> = related
                .iter()
                .filter_map(|(id, score)| {
                    notes.get(id).map(|n| NodeBrief {
                        id: *id,
                        kind: format!("{:?}", n.kind).to_lowercase(),
                        title: n.title.clone(),
                        desc: truncate_chars(&n.desc, MAX_NODE_DESC_CHARS),
                        score: *score,
                    })
                })
                .collect();
            let reason = if has_peer {
                "corroboration_cluster"
            } else if !related_nodes.is_empty() {
                "existing_node_touch"
            } else {
                "explicit_user_directive"
            };
            let briefs: Vec<ChunkBrief> = part
                .iter()
                .map(|c| ChunkBrief {
                    session_id: c.session_id.clone(),
                    chunk_id: c.chunk_id.clone(),
                    summary: c.summary.clone(),
                    explicit_directive: explicit_directive(c),
                    excerpt: c.summary.is_none().then(|| excerpt(c)),
                })
                .collect();
            let distinct_sessions = briefs.iter().map(|b| &b.session_id).collect::<BTreeSet<_>>().len();
            // Entities shared by two or more chunks of the packet.
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for b in &briefs {
                let text = format!("{}\n{}", b.summary.as_deref().unwrap_or(""), b.excerpt.as_deref().unwrap_or(""));
                for e in entities(&text) {
                    *counts.entry(e).or_default() += 1;
                }
            }
            let shared_entities: Vec<String> = counts.into_iter().filter(|(_, n)| *n >= 2).map(|(e, _)| e).take(20).collect();
            let mut chunk_pairs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for b in &briefs {
                for (k, v) in pairs(b.summary.as_deref().unwrap_or("")) {
                    chunk_pairs.entry(k).or_default().insert(v);
                }
            }
            let conflict_hint = related_nodes.iter().any(|n| {
                pairs(&n.desc).iter().any(|(k, v)| {
                    chunk_pairs.get(k).is_some_and(|values| values.iter().any(|cv| cv != v))
                })
            });
            let mut hasher = Sha256::new();
            for b in &briefs {
                hasher.update(b.chunk_id.as_bytes());
                hasher.update([0]);
                hasher.update(b.summary.as_deref().unwrap_or("").as_bytes());
                hasher.update([1]);
            }
            for n in &related_nodes {
                let stamp = notes.get(&n.id).map_or(0, |note| note.updated_at_ms);
                hasher.update(format!("note:{}:{}", n.id, stamp).as_bytes());
            }
            let hash: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
            packets.push(LinkedPacket {
                reason: reason.into(),
                chunks: briefs,
                distinct_sessions,
                shared_entities,
                related_nodes,
                conflict_hint,
                hash,
            });
        }
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;
    use memb_scroll::{ScrollMessage, Role};

    fn chunk(id: &str, summary: Option<&str>, user_text: &str, flag: Option<bool>) -> ScrollChunk {
        ScrollChunk {
            chunk_id: id.into(),
            session_id: id.split('_').nth(1).unwrap().into(),
            chunk_index: 0,
            messages: vec![ScrollMessage { role: Role::User, content: user_text.into(), ts_ms: 1 }],
            summary: summary.map(str::to_owned),
            created_at_ms: 1,
            curated: false,
            explicit_signal: flag,
            summary_vec: None,
        }
    }

    fn note(id: u64, kind: Kind, status: NoteStatus, desc: &str, updated: u64) -> Note {
        Note {
            id, title: format!("note {id}"), desc: desc.into(), kind, status, parents: vec![],
            properties: Default::default(), supersedes: None, created_at_ms: 1, updated_at_ms: updated,
        }
    }

    fn links(chunk_pairs: &[(&str, &str)], node_pairs: &[(&str, u64, f32)]) -> LinkSet {
        let mut set = LinkSet::default();
        for (a, b) in chunk_pairs {
            set.chunk_links.entry((*a).into()).or_default().insert((*b).into());
            set.chunk_links.entry((*b).into()).or_default().insert((*a).into());
        }
        for (c, n, s) in node_pairs {
            set.node_links.entry((*c).into()).or_default().push((*n, *s));
        }
        set
    }

    #[test]
    fn the_three_rules_decide_which_chunks_enter_a_packet() {
        let chunks = vec![
            chunk("chk_s1_0000", Some("- uses crates/memb/src/main.rs\n- retain_tail=10"), "one", None),
            chunk("chk_s2_0000", Some("- edits crates/memb/src/main.rs\n- retain_tail=12"), "two", None),
            chunk("chk_s3_0000", Some("- touches an existing note"), "three", None),
            chunk("chk_s4_0000", Some("- plain chatter"), "four", None),
            chunk("chk_s5_0000", None, "From now on use the release profile", None),
            chunk("chk_s6_0000", Some("- flagged"), "six", Some(true)),
        ];
        let notes: BTreeMap<u64, Note> = [
            (7, note(7, Kind::Knowledge, NoteStatus::Active, "retain_tail=10 keeps the overlap", 50)),
            (8, note(8, Kind::Topic, NoteStatus::Active, "a topic", 5)),
            (9, note(9, Kind::Claim, NoteStatus::Retracted, "gone", 5)),
        ].into_iter().collect();
        let l = links(
            &[("chk_s1_0000", "chk_s2_0000")],
            &[("chk_s1_0000", 7, 0.9), ("chk_s3_0000", 7, 0.8), ("chk_s3_0000", 8, 0.99), ("chk_s3_0000", 9, 0.99)],
        );
        let packets = build_linked_packets(&chunks, &l, &notes);
        let reasons: Vec<(&str, usize)> = packets.iter().map(|p| (p.reason.as_str(), p.chunks.len())).collect();
        assert_eq!(
            reasons,
            vec![("corroboration_cluster", 2), ("existing_node_touch", 1), ("explicit_user_directive", 1), ("explicit_user_directive", 1)],
            "s4 meets no rule and is in no packet"
        );
        let all: Vec<&str> = packets.iter().flat_map(|p| p.chunks.iter().map(|c| c.chunk_id.as_str())).collect();
        assert!(!all.contains(&"chk_s4_0000"));
        let cluster = &packets[0];
        assert_eq!(cluster.distinct_sessions, 2);
        assert_eq!(cluster.shared_entities, vec!["crates/memb/src/main.rs".to_string()]);
        assert_eq!(cluster.related_nodes.iter().map(|n| n.id).collect::<Vec<_>>(), vec![7]);
        assert!(cluster.conflict_hint, "retain_tail is 10 in the note, 10 and 12 in the chunks: disagreement");
        // Only active knowledge/claim notes are listed: topic 8 and retracted claim 9 are not.
        assert_eq!(packets[1].related_nodes.iter().map(|n| n.id).collect::<Vec<_>>(), vec![7]);
        assert!(!packets[1].conflict_hint);
        // Chunk without a summary: its directive is found in the user's own words and an excerpt is offered.
        assert!(packets[2].chunks[0].excerpt.as_deref().unwrap().contains("From now on"));
    }

    #[test]
    fn large_groups_split_and_the_hash_tracks_content_and_related_note_edits() {
        let chunks: Vec<ScrollChunk> = (0..7).map(|i| chunk(&format!("chk_s{i}_0000"), Some("- same"), "x", None)).collect();
        let ids: Vec<(String, String)> = (1..7).map(|i| ("chk_s0_0000".to_string(), format!("chk_s{i}_0000"))).collect();
        let pairs: Vec<(&str, &str)> = ids.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let notes: BTreeMap<u64, Note> = [(1, note(1, Kind::Knowledge, NoteStatus::Active, "d", 10))].into_iter().collect();
        let l = links(&pairs, &[("chk_s0_0000", 1, 0.9)]);
        let packets = build_linked_packets(&chunks, &l, &notes);
        assert_eq!(packets.iter().map(|p| p.chunks.len()).collect::<Vec<_>>(), vec![5, 2]);
        assert_eq!(packets[0].related_nodes.len(), 1);
        assert!(packets[1].related_nodes.is_empty(), "the second part has no node link of its own");
        // The same inputs hash the same; a changed summary or an edited related note changes it.
        let again = build_linked_packets(&chunks, &l, &notes);
        assert_eq!(packets[0].hash, again[0].hash);
        let mut changed = chunks.clone();
        changed[0].summary = Some("- different".into());
        assert_ne!(build_linked_packets(&changed, &l, &notes)[0].hash, packets[0].hash);
        let edited: BTreeMap<u64, Note> = [(1, note(1, Kind::Knowledge, NoteStatus::Active, "d", 11))].into_iter().collect();
        assert_ne!(build_linked_packets(&chunks, &l, &edited)[0].hash, packets[0].hash);
        assert_eq!(packets[0].hash.len(), 64);
    }
}
