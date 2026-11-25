use memb_core::{
    validate_desc, validate_parents_shape, validate_properties, validate_query, validate_title,
    Draft, Kind, Note, NotePatch, NoteStatus,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_NOTES: usize = 50_000;
const LEGACY_PROJECTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("projects_v1");
pub const NOTES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("notes_v2");
pub const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta_v2");
pub const REVISIONS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("revisions_v2");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub rev_id: u64,
    pub ts_ms: u64,
    pub op: String,
    pub note_id: u64,
    pub before: Option<Note>,
    pub after: Option<Note>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Counts {
    pub total: usize,
    pub topics: usize,
    pub knowledge: usize,
    pub claims: usize,
}

/// Exact legacy v0.3.0 `projects_v1` schema (`next`, not `next_id`, with alias fallback).
#[derive(Debug, Deserialize)]
struct LegacyProject {
    #[serde(alias = "next_id")]
    #[allow(dead_code)]
    next: u64,
    notes: BTreeMap<u64, LegacyNote>,
}

#[derive(Debug, Deserialize)]
struct LegacyNote {
    id: u64,
    title: String,
    #[serde(alias = "desc")]
    body: String,
    kind: Kind,
    #[serde(default)]
    parents: Vec<u64>,
}

/// RAII guard holding a real OS-level exclusive lock (Rust 1.89 std::fs::File::try_lock).
struct OsLockGuard {
    file: File,
}

impl Drop for OsLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn acquire_os_lock(root: &Path) -> Result<OsLockGuard, String> {
    fs::create_dir_all(root).map_err(|e| format!("create data dir: {e}"))?;
    let lock_path = root.join("store.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open lock file {}: {e}", lock_path.display()))?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(OsLockGuard { file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!("timed out waiting for OS lock on {}", lock_path.display()));
            }
            Err(TryLockError::Error(e)) => {
                return Err(format!("OS lock failed on {}: {e}", lock_path.display()));
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    db: Arc<Database>,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("MEMB_DATA_DIR must be an absolute path".into());
        }
        let _lock = acquire_os_lock(root)?;
        let db_path = root.join("memory.redb");
        let db = Database::builder()
            .set_cache_size(16 * 1024 * 1024)
            .create(&db_path)
            .map_err(|e| format!("open redb: {e}"))?;

        let store = Self {
            root: root.to_path_buf(),
            db: Arc::new(db),
        };
        store.init_and_migrate_locked()?;
        Ok(store)
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("memory.redb")
    }

    /// Fail-closed atomic migration from `projects_v1` -> `notes_v2`.
    /// - Reads legacy `next` field (with `next_id` alias).
    /// - Aborts the entire transaction (never setting `migrated_v1_to_v2`) if ANY project fails to deserialize
    ///   or if any migrated note count mismatches the legacy count.
    /// - Keeps `projects_v1` intact as a read-only backup.
    fn init_and_migrate_locked(&self) -> Result<(), String> {
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let _revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;

            let already_migrated = meta
                .get("migrated_v1_to_v2")
                .map_err(|e| e.to_string())?
                .is_some();

            if !already_migrated {
                let mut next_id: u64 = 1;
                let mut expected_notes: usize = 0;
                let mut migrated_notes: usize = 0;

                if let Ok(legacy) = w.open_table(LEGACY_PROJECTS_TABLE) {
                    let mut parsed_projects: Vec<(String, LegacyProject)> = Vec::new();
                    for item in legacy.iter().map_err(|e| e.to_string())? {
                        let (k, v) = item.map_err(|e| e.to_string())?;
                        let slug = k.value().to_string();
                        let lp: LegacyProject = serde_json::from_slice(v.value()).map_err(|e| {
                            format!(
                                "aborting v1->v2 migration: failed to deserialize legacy project '{slug}': {e}"
                            )
                        })?;
                        expected_notes += lp.notes.len();
                        parsed_projects.push((slug, lp));
                    }

                    let ts = now_ms();
                    for (slug, lp) in parsed_projects {
                        let mut id_remap = BTreeMap::new();
                        for (&old_id, _) in &lp.notes {
                            id_remap.insert(old_id, next_id);
                            next_id += 1;
                        }
                        for (old_id, ln) in lp.notes {
                            let new_id = id_remap[&old_id];
                            let mut parents = Vec::with_capacity(ln.parents.len());
                            for pid in ln.parents {
                                let mapped = id_remap.get(&pid).copied().ok_or_else(|| {
                                    format!(
                                        "aborting v1->v2 migration: legacy project '{slug}' note {} references missing parent {}",
                                        ln.id, pid
                                    )
                                })?;
                                parents.push(mapped);
                            }
                            let mut properties = BTreeMap::new();
                            properties.insert("legacy_project".into(), json!(slug));
                            properties.insert("legacy_id".into(), json!(ln.id));

                            let note = Note {
                                id: new_id,
                                title: ln.title,
                                desc: ln.body,
                                kind: ln.kind,
                                status: NoteStatus::Active,
                                parents,
                                properties,
                                supersedes: None,
                                created_at_ms: ts,
                                updated_at_ms: ts,
                            };
                            let bytes = serde_json::to_vec(&note).map_err(|e| e.to_string())?;
                            notes.insert(new_id, bytes.as_slice()).map_err(|e| e.to_string())?;
                            migrated_notes += 1;
                        }
                    }
                }

                if migrated_notes != expected_notes {
                    return Err(format!(
                        "aborting v1->v2 migration: count mismatch (expected {expected_notes}, migrated {migrated_notes})"
                    ));
                }

                if meta.get("next_id").map_err(|e| e.to_string())?.is_none() {
                    meta.insert("next_id", next_id.to_le_bytes().as_slice())
                        .map_err(|e| e.to_string())?;
                }
                if meta.get("next_rev").map_err(|e| e.to_string())?.is_none() {
                    meta.insert("next_rev", 1u64.to_le_bytes().as_slice())
                        .map_err(|e| e.to_string())?;
                }
                meta.insert("migrated_v1_to_v2", b"1".as_slice())
                    .map_err(|e| e.to_string())?;
            }
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_all_notes(&self) -> Result<BTreeMap<u64, Note>, String> {
        let r = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = r.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        for item in table.iter().map_err(|e| e.to_string())? {
            let (k, v) = item.map_err(|e| e.to_string())?;
            let note: Note = serde_json::from_slice(v.value()).map_err(|e| e.to_string())?;
            out.insert(k.value(), note);
        }
        Ok(out)
    }

    fn verify_parents_exist(
        all: &BTreeMap<u64, Note>,
        kind: Kind,
        parents: &[u64],
    ) -> Result<(), String> {
        let required = match kind {
            Kind::Topic => return Ok(()),
            Kind::Knowledge => Kind::Topic,
            Kind::Claim => Kind::Knowledge,
        };
        for &pid in parents {
            let parent = all
                .get(&pid)
                .ok_or_else(|| format!("parent note {pid} does not exist"))?;
            if parent.status != NoteStatus::Active {
                return Err(format!("parent note {pid} is not active"));
            }
            if parent.kind != required {
                return Err(format!(
                    "{:?} parent {pid} must have kind {:?}, found {:?}",
                    kind, required, parent.kind
                ));
            }
        }
        Ok(())
    }

    fn next_counter(meta: &mut redb::Table<&str, &[u8]>, key: &str) -> Result<u64, String> {
        let cur = meta
            .get(key)
            .map_err(|e| e.to_string())?
            .map(|v| {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&v.value()[..8]);
                u64::from_le_bytes(arr)
            })
            .unwrap_or(1);
        let next = cur.checked_add(1).ok_or("counter overflow")?;
        meta.insert(key, next.to_le_bytes().as_slice())
            .map_err(|e| e.to_string())?;
        Ok(cur)
    }

    pub fn remember(&self, draft: Draft) -> Result<Note, String> {
        let draft = draft.validate().map_err(|e| e.to_string())?;
        let _lock = acquire_os_lock(&self.root)?;
        let existing = self.load_all_notes()?;
        if existing.len() >= MAX_NOTES {
            return Err(format!("store reached MAX_NOTES ({MAX_NOTES})"));
        }
        Self::verify_parents_exist(&existing, draft.kind, &draft.parents)?;

        let ts = now_ms();
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        let note = {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let id = Self::next_counter(&mut meta, "next_id")?;
            let rev_id = Self::next_counter(&mut meta, "next_rev")?;

            let note = Note {
                id,
                title: draft.title,
                desc: draft.desc,
                kind: draft.kind,
                status: NoteStatus::Active,
                parents: draft.parents,
                properties: draft.properties,
                supersedes: None,
                created_at_ms: ts,
                updated_at_ms: ts,
            };
            let bytes = serde_json::to_vec(&note).map_err(|e| e.to_string())?;
            notes.insert(id, bytes.as_slice()).map_err(|e| e.to_string())?;

            let rev = Revision {
                rev_id,
                ts_ms: ts,
                op: "remember".into(),
                note_id: id,
                before: None,
                after: Some(note.clone()),
            };
            let rbytes = serde_json::to_vec(&rev).map_err(|e| e.to_string())?;
            revs.insert(rev_id, rbytes.as_slice()).map_err(|e| e.to_string())?;
            note
        };
        w.commit().map_err(|e| e.to_string())?;
        Ok(note)
    }

    pub fn update(&self, id: u64, patch: NotePatch) -> Result<Note, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let existing = self.load_all_notes()?;
        let before = existing
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("note {id} not found"))?;
        if before.status != NoteStatus::Active {
            return Err(format!("cannot update non-active note {id}"));
        }

        let mut after = before.clone();
        if let Some(t) = patch.title {
            after.title = validate_title(&t).map_err(|e| e.to_string())?;
        }
        if let Some(d) = patch.desc {
            after.desc = validate_desc(&d).map_err(|e| e.to_string())?;
        }
        if let Some(ps) = patch.parents {
            let valid = validate_parents_shape(after.kind, &ps).map_err(|e| e.to_string())?;
            Self::verify_parents_exist(&existing, after.kind, &valid)?;
            after.parents = valid;
        }
        if let Some(pmap) = patch.properties {
            for (k, v) in pmap {
                if v.is_null() {
                    after.properties.remove(&k);
                } else {
                    after.properties.insert(k, v);
                }
            }
            validate_properties(&after.properties).map_err(|e| e.to_string())?;
        }
        after.updated_at_ms = now_ms();

        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let rev_id = Self::next_counter(&mut meta, "next_rev")?;
            let bytes = serde_json::to_vec(&after).map_err(|e| e.to_string())?;
            notes.insert(id, bytes.as_slice()).map_err(|e| e.to_string())?;
            let rev = Revision {
                rev_id,
                ts_ms: after.updated_at_ms,
                op: "update".into(),
                note_id: id,
                before: Some(before),
                after: Some(after.clone()),
            };
            let rbytes = serde_json::to_vec(&rev).map_err(|e| e.to_string())?;
            revs.insert(rev_id, rbytes.as_slice()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(after)
    }

    pub fn supersede(&self, old_id: u64, draft: Draft) -> Result<Note, String> {
        let draft = draft.validate().map_err(|e| e.to_string())?;
        let _lock = acquire_os_lock(&self.root)?;
        let existing = self.load_all_notes()?;
        let mut old_note = existing
            .get(&old_id)
            .cloned()
            .ok_or_else(|| format!("note {old_id} not found"))?;
        if old_note.kind != Kind::Claim || draft.kind != Kind::Claim {
            return Err("supersede is restricted to claim notes".into());
        }
        Self::verify_parents_exist(&existing, draft.kind, &draft.parents)?;

        let ts = now_ms();
        let before_old = old_note.clone();
        old_note.status = NoteStatus::Superseded;
        old_note.updated_at_ms = ts;

        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        let new_note = {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let new_id = Self::next_counter(&mut meta, "next_id")?;
            let rev_old = Self::next_counter(&mut meta, "next_rev")?;
            let rev_new = Self::next_counter(&mut meta, "next_rev")?;

            let new_note = Note {
                id: new_id,
                title: draft.title,
                desc: draft.desc,
                kind: Kind::Claim,
                status: NoteStatus::Active,
                parents: draft.parents,
                properties: draft.properties,
                supersedes: Some(old_id),
                created_at_ms: ts,
                updated_at_ms: ts,
            };

            let ob = serde_json::to_vec(&old_note).map_err(|e| e.to_string())?;
            let nb = serde_json::to_vec(&new_note).map_err(|e| e.to_string())?;
            notes.insert(old_id, ob.as_slice()).map_err(|e| e.to_string())?;
            notes.insert(new_id, nb.as_slice()).map_err(|e| e.to_string())?;

            let r1 = serde_json::to_vec(&Revision {
                rev_id: rev_old,
                ts_ms: ts,
                op: "supersede_old".into(),
                note_id: old_id,
                before: Some(before_old),
                after: Some(old_note),
            }).map_err(|e| e.to_string())?;
            let r2 = serde_json::to_vec(&Revision {
                rev_id: rev_new,
                ts_ms: ts,
                op: "supersede_new".into(),
                note_id: new_id,
                before: None,
                after: Some(new_note.clone()),
            }).map_err(|e| e.to_string())?;
            revs.insert(rev_old, r1.as_slice()).map_err(|e| e.to_string())?;
            revs.insert(rev_new, r2.as_slice()).map_err(|e| e.to_string())?;
            new_note
        };
        w.commit().map_err(|e| e.to_string())?;
        Ok(new_note)
    }

    pub fn retract(
        &self,
        id: u64,
        migrate_to_topic_id: Option<u64>,
        force_cascade: bool,
    ) -> Result<Value, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let all = self.load_all_notes()?;
        let target = all
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("note {id} not found"))?;
        let ts = now_ms();
        let mut mutated: Vec<(Note, Note)> = Vec::new();

        match target.kind {
            Kind::Claim => {
                let mut updated = target.clone();
                updated.status = NoteStatus::Retracted;
                updated.updated_at_ms = ts;
                mutated.push((target, updated));
            }
            Kind::Knowledge => {
                let mut updated = target.clone();
                updated.status = NoteStatus::Retracted;
                updated.updated_at_ms = ts;
                mutated.push((target, updated));

                for child in all.values().filter(|n| {
                    n.status == NoteStatus::Active && n.kind == Kind::Claim && n.parents.contains(&id)
                }) {
                    let mut next = child.clone();
                    next.parents.retain(|&p| p != id);
                    if next.parents.is_empty() {
                        next.status = NoteStatus::Retracted;
                    }
                    next.updated_at_ms = ts;
                    mutated.push((child.clone(), next));
                }
            }
            Kind::Topic => {
                let sole_children: Vec<Note> = all
                    .values()
                    .filter(|n| {
                        n.status == NoteStatus::Active
                            && n.kind == Kind::Knowledge
                            && n.parents == vec![id]
                    })
                    .cloned()
                    .collect();

                if !sole_children.is_empty() && migrate_to_topic_id.is_none() && !force_cascade {
                    return Err(format!(
                        "topic {id} has {} sole-linked knowledge note(s); pass migrate_to_topic_id or force_cascade=true",
                        sole_children.len()
                    ));
                }

                if let Some(dest) = migrate_to_topic_id {
                    let dest_topic = all
                        .get(&dest)
                        .ok_or_else(|| format!("migrate_to_topic_id {dest} does not exist"))?;
                    if dest == id || dest_topic.kind != Kind::Topic || dest_topic.status != NoteStatus::Active {
                        return Err("migrate_to_topic_id must reference a different active topic".into());
                    }
                    for child in all.values().filter(|n| {
                        n.status == NoteStatus::Active && n.kind == Kind::Knowledge && n.parents.contains(&id)
                    }) {
                        let mut next = child.clone();
                        next.parents.retain(|&p| p != id);
                        if !next.parents.contains(&dest) {
                            next.parents.push(dest);
                        }
                        next.updated_at_ms = ts;
                        mutated.push((child.clone(), next));
                    }
                } else {
                    let mut retracted_k = BTreeSet::new();
                    for child in all.values().filter(|n| {
                        n.status == NoteStatus::Active && n.kind == Kind::Knowledge && n.parents.contains(&id)
                    }) {
                        let mut next = child.clone();
                        next.parents.retain(|&p| p != id);
                        if next.parents.is_empty() {
                            next.status = NoteStatus::Retracted;
                            retracted_k.insert(next.id);
                        }
                        next.updated_at_ms = ts;
                        mutated.push((child.clone(), next));
                    }
                    for claim in all.values().filter(|n| {
                        n.status == NoteStatus::Active
                            && n.kind == Kind::Claim
                            && n.parents.iter().any(|p| retracted_k.contains(p))
                    }) {
                        let mut next = claim.clone();
                        next.parents.retain(|p| !retracted_k.contains(p));
                        if next.parents.is_empty() {
                            next.status = NoteStatus::Retracted;
                        }
                        next.updated_at_ms = ts;
                        mutated.push((claim.clone(), next));
                    }
                }

                let mut updated = target.clone();
                updated.status = NoteStatus::Retracted;
                updated.updated_at_ms = ts;
                mutated.push((target, updated));
            }
        }

        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            for (before, after) in &mutated {
                let rev_id = Self::next_counter(&mut meta, "next_rev")?;
                let bytes = serde_json::to_vec(after).map_err(|e| e.to_string())?;
                notes.insert(after.id, bytes.as_slice()).map_err(|e| e.to_string())?;
                let rbytes = serde_json::to_vec(&Revision {
                    rev_id,
                    ts_ms: ts,
                    op: "retract".into(),
                    note_id: after.id,
                    before: Some(before.clone()),
                    after: Some(after.clone()),
                }).map_err(|e| e.to_string())?;
                revs.insert(rev_id, rbytes.as_slice()).map_err(|e| e.to_string())?;
            }
        }
        w.commit().map_err(|e| e.to_string())?;

        Ok(json!({
            "retracted_root": id,
            "affected_notes": mutated.iter().map(|(_, a)| a.id).collect::<Vec<_>>()
        }))
    }

    pub fn inspect(&self, id: u64) -> Result<Option<Note>, String> {
        let all = self.load_all_notes()?;
        Ok(all.get(&id).cloned())
    }

    pub fn search_lexical_ids(&self, query: &str, limit: usize) -> Result<Vec<u64>, String> {
        let q = validate_query(query).map_err(|e| e.to_string())?;
        let terms = tokenize(&q);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let all = self.load_all_notes()?;
        let mut scored: Vec<(usize, u64)> = all
            .values()
            .filter(|n| n.status == NoteStatus::Active)
            .filter_map(|n| {
                let title_hits = terms.intersection(&tokenize(&n.title)).count();
                let desc_hits = terms.intersection(&tokenize(&n.desc)).count();
                let score = 3 * title_hits + desc_hits;
                (score > 0).then_some((score, n.id))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(scored.into_iter().take(limit).map(|(_, id)| id).collect())
    }

    pub fn expand_one_hop(&self, seed_ids: &[u64], max_total: usize) -> Result<Vec<Note>, String> {
        let all = self.load_all_notes()?;
        let mut selected: Vec<u64> = seed_ids.to_vec();
        for &id in seed_ids {
            if selected.len() >= max_total {
                break;
            }
            if let Some(n) = all.get(&id) {
                match n.kind {
                    Kind::Claim => {
                        for &pid in &n.parents {
                            if selected.len() >= max_total { break; }
                            if !selected.contains(&pid) { selected.push(pid); }
                        }
                    }
                    Kind::Knowledge => {
                        for c in all.values().filter(|c| {
                            c.status == NoteStatus::Active
                                && c.kind == Kind::Claim
                                && c.parents.contains(&id)
                        }) {
                            if selected.len() >= max_total { break; }
                            if !selected.contains(&c.id) { selected.push(c.id); }
                        }
                    }
                    Kind::Topic => {}
                }
            }
        }
        Ok(selected.into_iter().filter_map(|id| all.get(&id).cloned()).collect())
    }

    pub fn search(&self, query: &str) -> Result<Vec<Note>, String> {
        let direct = self.search_lexical_ids(query, 5)?;
        self.expand_one_hop(&direct, 8)
    }

    pub fn snapshot(&self, query: Option<&str>, after: Option<u64>) -> Result<Value, String> {
        let all = self.load_all_notes()?;
        let q = query.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
        let active: Vec<&Note> = all.values().filter(|n| n.status == NoteStatus::Active).collect();
        let counts = count_notes(&active);

        let filtered: Vec<&Note> = active
            .into_iter()
            .filter(|n| after.map(|a| n.id > a).unwrap_or(true))
            .filter(|n| {
                q.as_ref().map_or(true, |needle| {
                    n.title.to_lowercase().contains(needle) || n.desc.to_lowercase().contains(needle)
                })
            })
            .collect();

        let has_more = filtered.len() > 12;
        let page: Vec<&Note> = filtered.into_iter().take(12).collect();
        let next = if has_more { page.last().map(|n| n.id.to_string()) } else { None };
        Ok(json!({ "counts": counts, "notes": page, "next": next }))
    }

    pub fn graph(&self, include_inactive: bool) -> Result<Value, String> {
        let all = self.load_all_notes()?;
        let list: Vec<&Note> = all
            .values()
            .filter(|n| include_inactive || n.status == NoteStatus::Active)
            .collect();
        let counts = count_notes(&list);
        let notes: Vec<Value> = list
            .into_iter()
            .map(|n| {
                json!({
                    "id": n.id.to_string(),
                    "title": n.title,
                    "desc": n.desc,
                    "kind": n.kind,
                    "status": n.status,
                    "parents": n.parents.iter().map(u64::to_string).collect::<Vec<_>>(),
                    "properties": n.properties,
                    "supersedes": n.supersedes.map(|s| s.to_string()),
                    "created_at_ms": n.created_at_ms,
                    "updated_at_ms": n.updated_at_ms,
                })
            })
            .collect();
        Ok(json!({ "notes": notes, "counts": counts }))
    }
}

fn count_notes(list: &[&Note]) -> Counts {
    let mut c = Counts { total: list.len(), topics: 0, knowledge: 0, claims: 0 };
    for n in list {
        match n.kind {
            Kind::Topic => c.topics += 1,
            Kind::Knowledge => c.knowledge += 1,
            Kind::Claim => c.claims += 1,
        }
    }
    c
}

fn tokenize(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("memb_test_{}_{}", tag, now_ms()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn migrates_legacy_projects_v1_with_next_field_losslessly() {
        let dir = temp_dir("mig_ok");
        {
            let db = Database::create(dir.join("memory.redb")).unwrap();
            let w = db.begin_write().unwrap();
            {
                let mut tbl = w.open_table(LEGACY_PROJECTS_TABLE).unwrap();
                let payload = serde_json::json!({
                    "next": 3,
                    "notes": {
                        "1": {"id": 1, "title": "Rust", "body": "Systems language", "kind": "topic", "parents": []},
                        "2": {"id": 2, "title": "redb", "body": "Embedded KV store", "kind": "knowledge", "parents": [1]}
                    }
                });
                let bytes = serde_json::to_vec(&payload).unwrap();
                tbl.insert("alpha", bytes.as_slice()).unwrap();
            }
            w.commit().unwrap();
        }

        let store = Store::open(&dir).expect("migration must succeed");
        let all = store.load_all_notes().unwrap();
        assert_eq!(all.len(), 2);
        let k = all.values().find(|n| n.kind == Kind::Knowledge).unwrap();
        assert_eq!(k.desc, "Embedded KV store");
        assert_eq!(k.properties.get("legacy_project"), Some(&json!("alpha")));
        assert_eq!(k.parents.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aborts_migration_on_corrupt_legacy_project_without_marking_complete() {
        let dir = temp_dir("mig_fail");
        {
            let db = Database::create(dir.join("memory.redb")).unwrap();
            let w = db.begin_write().unwrap();
            {
                let mut tbl = w.open_table(LEGACY_PROJECTS_TABLE).unwrap();
                tbl.insert("broken", b"{not valid json".as_slice()).unwrap();
            }
            w.commit().unwrap();
        }

        let err = Store::open(&dir).expect_err("must fail closed on corrupt legacy project");
        assert!(err.contains("aborting v1->v2 migration"));
        let _ = fs::remove_dir_all(&dir);
    }
}
