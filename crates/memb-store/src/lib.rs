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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_NOTES: usize = 50_000;
const LEGACY_PROJECTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("projects_v1");
const LEGACY_SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("legacy_snapshot_v2");
pub const NOTES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("notes_v2");
pub const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta_v2");
/// Knowledge note id -> ids of its active claim children (JSON array, ascending). A derived
/// index: it is valid for exactly the `kb_version` recorded under `adjacency_version`.
const ADJACENCY_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("adjacency_v1");
/// The most claims one knowledge note adds to a one-hop expansion, so a single busy
/// knowledge note cannot fill the whole result.
pub const CLAIMS_PER_KNOWLEDGE: usize = 4;
#[cfg(test)]
thread_local! {
    static SKIP_PUBLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SLOW_VERSION_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
thread_local! {
    static FULL_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times a store has read every note on the calling thread (a counter for tests
/// and diagnostics: retrieval that is meant to avoid full scans can be checked against it).
pub fn full_scan_count() -> usize {
    FULL_SCANS.with(|c| c.get())
}
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

/// An open database handle that is only ever held under the store lock. When it is closed
/// it reads the committed version, closes the database, and records the version with the
/// database's final length and modification time in `kb.version` (see `Store::kb_version`),
/// so the record is written in the same critical section as the commit it describes.
struct StoreDb<'a> {
    db: Option<Database>,
    store: &'a Store,
}

impl std::ops::Deref for StoreDb<'_> {
    type Target = Database;
    fn deref(&self) -> &Database {
        self.db.as_ref().expect("database handle is open until dropped")
    }
}

impl std::ops::DerefMut for StoreDb<'_> {
    fn deref_mut(&mut self) -> &mut Database {
        self.db.as_mut().expect("database handle is open until dropped")
    }
}

impl Drop for StoreDb<'_> {
    fn drop(&mut self) {
        let Some(db) = self.db.take() else { return };
        #[cfg(test)]
        if SKIP_PUBLISH.with(|c| c.get()) {
            return;
        }
        let version = Store::kb_version_locked(&db);
        drop(db); // closing rewrites redb's header: stamp the final state
        match version {
            Ok(v) => self.store.write_version_file(v),
            Err(_) => {
                let _ = fs::remove_file(self.store.version_path());
            }
        }
    }
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    max_notes: usize,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("MEMB_DATA_DIR must be an absolute path".into());
        }
        let store = Self { root: root.to_path_buf(), max_notes: MAX_NOTES };
        let _lock = acquire_os_lock(root)?;
        let db = store.open_db()?;
        Self::init_and_migrate_locked(&db)?;
        drop(db); // publishes the version record (under the lock) before `store` moves
        Ok(store)
    }

    /// Lower the note limit (config.toml `storage.max_notes`). It can never exceed
    /// the built-in `MAX_NOTES`, and notes beyond a lowered limit are kept, not removed.
    pub fn with_max_notes(mut self, max_notes: usize) -> Self {
        self.max_notes = max_notes.clamp(1, MAX_NOTES);
        self
    }

    fn open_db(&self) -> Result<StoreDb<'_>, String> {
        let db = Database::builder().set_cache_size(16 * 1024 * 1024)
            .create(self.db_path()).map_err(|e| format!("open redb: {e}"))?;
        Self::verify_legacy(&db)?;
        Ok(StoreDb { db: Some(db), store: self })
    }

    fn verify_legacy(db: &Database) -> Result<(), String> {
        let r = db.begin_read().map_err(|e| e.to_string())?;
        let meta = match r.open_table(META_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if meta.get("migrated_v1_to_v2").map_err(|e| e.to_string())?.is_none() { return Ok(()); }
        let legacy = r.open_table(LEGACY_PROJECTS_TABLE).map_err(|e| format!("legacy table unavailable: {e}"))?;
        let snapshot = r.open_table(LEGACY_SNAPSHOT).map_err(|e| format!("legacy snapshot unavailable: {e}"))?;
        let mut a = legacy.iter().map_err(|e| e.to_string())?;
        let mut b = snapshot.iter().map_err(|e| e.to_string())?;
        loop {
            match (a.next(), b.next()) {
                (None, None) => break,
                (Some(Ok((ak, av))), Some(Ok((bk, bv)))) if ak.value() == bk.value() && av.value() == bv.value() => {},
                _ => return Err("legacy projects changed since migration; refusing access".into()),
            }
        }
        Ok(())
    }

    /// Run a trusted extension against the database while holding the store lock.
    /// The callback may access only its own tables; it must not change the store's
    /// legacy, snapshot, notes, metadata, or revision tables. It must not call a
    /// Store method (the lock is not reentrant), retain a database handle, or
    /// start work that outlives the callback. Callback commits are not rolled back
    /// if it returns an error or violates this contract.
    pub fn with_db<T>(&self, f: impl FnOnce(&Database) -> Result<T, String>) -> Result<T, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let result = f(&db);
        {
            let read = db.begin_read().map_err(|e| e.to_string())?;
            let meta = read.open_table(META_TABLE).map_err(|e| format!("store metadata unavailable: {e}"))?;
            if meta.get("migrated_v1_to_v2").map_err(|e| e.to_string())?.is_none() {
                return Err("migration marker removed by database callback".into());
            }
        }
        Self::verify_legacy(&db)?;
        result
    }

    /// Compacts `memory.redb` (Phase 4 storage hygiene) while holding the store lock.
    /// Returns the file size before and after. Notes, revisions and every other table
    /// are unchanged; only unused pages are returned to the file system.
    pub fn compact(&self) -> Result<(u64, u64), String> {
        let _lock = acquire_os_lock(&self.root)?;
        let mut db = self.open_db()?;
        let before = fs::metadata(self.db_path()).map(|m| m.len()).unwrap_or(0);
        db.compact().map_err(|e| format!("compact redb: {e}"))?;
        drop(db);
        let after = fs::metadata(self.db_path()).map(|m| m.len()).unwrap_or(0);
        Ok((before, after))
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
    fn init_and_migrate_locked(db: &Database) -> Result<(), String> {
        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let _revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;

            let already_migrated = meta
                .get("migrated_v1_to_v2")
                .map_err(|e| e.to_string())?
                .is_some();

            if !already_migrated {
                if notes.iter().map_err(|e| e.to_string())?.next().is_some()
                    || meta.iter().map_err(|e| e.to_string())?.next().is_some()
                    || _revs.iter().map_err(|e| e.to_string())?.next().is_some() {
                    return Err("aborting migration: existing v2 data without migration marker".into());
                }
                let mut next_id: u64 = 1;
                let mut expected_notes: usize = 0;
                let mut migrated_notes: usize = 0;

                {
                    let legacy = w.open_table(LEGACY_PROJECTS_TABLE).map_err(|e| e.to_string())?;
                    let mut snapshot = w.open_table(LEGACY_SNAPSHOT).map_err(|e| e.to_string())?;
                    let mut parsed_projects: Vec<(String, LegacyProject)> = Vec::new();
                    for item in legacy.iter().map_err(|e| e.to_string())? {
                        let (k, v) = item.map_err(|e| e.to_string())?;
                        snapshot.insert(k.value(), v.value()).map_err(|e| e.to_string())?;
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
                        for (&old_id, note) in &lp.notes {
                            if old_id != note.id { return Err(format!("aborting migration: project '{slug}' note key/id mismatch")); }
                            id_remap.insert(old_id, next_id);
                            next_id = next_id.checked_add(1).ok_or("migration ID overflow")?;
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
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        Self::load_all_notes_locked(&db)
    }

    fn load_all_notes_locked(db: &Database) -> Result<BTreeMap<u64, Note>, String> {
        FULL_SCANS.with(|c| c.set(c.get() + 1));
        let r = db.begin_read().map_err(|e| e.to_string())?;
        let table = r.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        for item in table.iter().map_err(|e| e.to_string())? {
            let (k, v) = item.map_err(|e| e.to_string())?;
            let note: Note = serde_json::from_slice(v.value()).map_err(|e| e.to_string())?;
            if k.value() != note.id { return Err("note key/id mismatch".into()); }
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
        let cur = match meta.get(key).map_err(|e| e.to_string())? {
            Some(v) => u64::from_le_bytes(v.value().try_into().map_err(|_| format!("invalid {key} counter"))?),
            None => return Err(format!("missing {key} counter")),
        };
        if cur == 0 { return Err(format!("invalid {key} counter")); }
        let next = cur.checked_add(1).ok_or("counter overflow")?;
        meta.insert(key, next.to_le_bytes().as_slice())
            .map_err(|e| e.to_string())?;
        Ok(cur)
    }

    pub fn remember(&self, draft: Draft) -> Result<Note, String> {
        let draft = draft.validate().map_err(|e| e.to_string())?;
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let existing = Self::load_all_notes_locked(&db)?;
        if existing.len() >= self.max_notes {
            return Err(format!("store reached its note limit ({})", self.max_notes));
        }
        Self::verify_parents_exist(&existing, draft.kind, &draft.parents)?;

        let ts = now_ms();
        let w = db.begin_write().map_err(|e| e.to_string())?;
        let note = {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let id = Self::next_counter(&mut meta, "next_id")?;
            let rev_id = Self::next_counter(&mut meta, "next_rev")?;
            if notes.get(id).map_err(|e| e.to_string())?.is_some() || revs.get(rev_id).map_err(|e| e.to_string())?.is_some() { return Err("counter collision".into()); }

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
        let db = self.open_db()?;
        let existing = Self::load_all_notes_locked(&db)?;
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

        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let rev_id = Self::next_counter(&mut meta, "next_rev")?;
            if revs.get(rev_id).map_err(|e| e.to_string())?.is_some() { return Err("revision counter collision".into()); }
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

    /// Read one revision without changing the store. Any preview composed with
    /// a separate note read is advisory; rollback independently rechecks state.
    pub fn revision(&self, id: u64) -> Result<Option<Revision>, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let read = db.begin_read().map_err(|e| e.to_string())?;
        let table = read.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
        let revision = table.get(id).map_err(|e| e.to_string())?
            .map(|value| serde_json::from_slice::<Revision>(value.value()).map_err(|e| e.to_string()))
            .transpose()?;
        if revision.as_ref().is_some_and(|rev| rev.rev_id != id) {
            return Err("revision key/id mismatch".into());
        }
        Ok(revision)
    }

    /// Newest-first revision history (read-only), optionally for one note. `limit` is capped at 200.
    pub fn list_revisions(&self, note_id: Option<u64>, limit: usize) -> Result<Vec<Revision>, String> {
        let limit = limit.clamp(1, 200);
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let read = db.begin_read().map_err(|e| e.to_string())?;
        let table = match read.open_table(REVISIONS_TABLE) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        let mut found = Vec::new();
        for entry in table.iter().map_err(|e| e.to_string())?.rev() {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let revision: Revision = serde_json::from_slice(value.value()).map_err(|e| e.to_string())?;
            if revision.rev_id != key.value() {
                return Err("revision key/id mismatch".into());
            }
            if note_id.is_none_or(|id| id == revision.note_id) {
                found.push(revision);
                if found.len() == limit {
                    break;
                }
            }
        }
        Ok(found)
    }

    /// Reverse a single update if its after-image is still current. Operations
    /// involving creation, deletion, supersession or cascades are unsupported.
    pub fn rollback_revision(&self, revision_id: u64) -> Result<Note, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let mut all = Self::load_all_notes_locked(&db)?;
        let revision: Revision = {
            let r = db.begin_read().map_err(|e| e.to_string())?;
            let revs = r.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let value = revs.get(revision_id).map_err(|e| e.to_string())?
                .ok_or_else(|| format!("revision {revision_id} not found"))?;
            serde_json::from_slice(value.value()).map_err(|e| e.to_string())?
        };
        if revision.rev_id != revision_id || revision.op != "update" {
            return Err("unsupported or inconsistent revision; only single-note updates can be rolled back".into());
        }
        let before = revision.before.ok_or("revision has no before-image")?;
        let after = revision.after.ok_or("revision has no after-image")?;
        if before.id != revision.note_id || after.id != revision.note_id
            || before.kind != after.kind || before.status != NoteStatus::Active
            || after.status != NoteStatus::Active || before.created_at_ms != after.created_at_ms
            || before.supersedes != after.supersedes {
            return Err("inconsistent update revision".into());
        }
        if all.get(&revision.note_id) != Some(&after) {
            return Err("rollback conflict: current note differs from revision after-image".into());
        }
        let draft = Draft { title: before.title.clone(), desc: before.desc.clone(), kind: before.kind,
            parents: before.parents.clone(), properties: before.properties.clone() };
        if draft.clone().validate().map_err(|e| e.to_string())? != draft {
            return Err("before-image does not satisfy current validation".into());
        }
        let mut restored = before;
        restored.updated_at_ms = now_ms();
        all.insert(restored.id, restored.clone());
        for note in all.values().filter(|n| n.status == NoteStatus::Active) {
            validate_parents_shape(note.kind, &note.parents).map_err(|e| e.to_string())?;
            Self::verify_parents_exist(&all, note.kind, &note.parents)?;
        }
        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let new_id = Self::next_counter(&mut meta, "next_rev")?;
            if revs.get(new_id).map_err(|e| e.to_string())?.is_some() { return Err("revision counter collision".into()); }
            let bytes = serde_json::to_vec(&restored).map_err(|e| e.to_string())?;
            notes.insert(restored.id, bytes.as_slice()).map_err(|e| e.to_string())?;
            let record = Revision { rev_id: new_id, ts_ms: restored.updated_at_ms,
                op: format!("rollback_update:{revision_id}"), note_id: restored.id,
                before: Some(after), after: Some(restored.clone()) };
            let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
            revs.insert(new_id, bytes.as_slice()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(restored)
    }

    /// Highest revision id so far (0 when there is none).
    pub fn latest_revision_id(&self) -> Result<u64, String> {
        Ok(self.list_revisions(None, 1)?.first().map_or(0, |r| r.rev_id))
    }

    /// Reverse a group of revisions atomically, newest first, all or nothing.
    ///
    /// Supported: `update`, `remember`, `supersede_new`, `supersede_old` and `retract`.
    /// A created note is not deleted: it becomes `Retracted`. A superseded or retracted
    /// note becomes active again with its earlier content. Every step needs the note to
    /// still equal that revision's after-image (no later edit), and the graph must stay
    /// valid (for example a created topic cannot be undone while an active note still
    /// hangs under it). Each step records its own `rollback_<op>:<id>` revision.
    pub fn rollback_revisions(&self, ids: &[u64]) -> Result<Vec<Note>, String> {
        if ids.is_empty() || ids.len() > 200 {
            return Err("give between 1 and 200 revisions".into());
        }
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let mut all = Self::load_all_notes_locked(&db)?;
        let mut revisions = Vec::new();
        {
            let r = db.begin_read().map_err(|e| e.to_string())?;
            let revs = r.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            for id in ids.iter().rev() {
                let value = revs.get(*id).map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("revision {id} not found"))?;
                let rev: Revision = serde_json::from_slice(value.value()).map_err(|e| e.to_string())?;
                if rev.rev_id != *id {
                    return Err("revision key/id mismatch".into());
                }
                revisions.push(rev);
            }
        }
        let ts = now_ms();
        let mut steps: Vec<(u64, String, Note, Note)> = Vec::new(); // (note, op, from, to)
        for rev in &revisions {
            let after = rev.after.clone().ok_or("revision has no after-image")?;
            if after.id != rev.note_id {
                return Err("inconsistent revision".into());
            }
            if all.get(&rev.note_id) != Some(&after) {
                return Err(format!("rollback conflict: note {} differs from revision {} after-image", rev.note_id, rev.rev_id));
            }
            let mut to = match rev.op.as_str() {
                "remember" | "supersede_new" => {
                    if rev.before.is_some() { return Err("inconsistent creation revision".into()); }
                    let mut n = after.clone();
                    n.status = NoteStatus::Retracted;
                    n
                }
                "update" | "supersede_old" | "retract" => {
                    let before = rev.before.clone().ok_or("revision has no before-image")?;
                    if before.id != rev.note_id || before.kind != after.kind || before.created_at_ms != after.created_at_ms {
                        return Err("inconsistent revision".into());
                    }
                    before
                }
                _ => return Err(format!("revision {} (`{}`) cannot be rolled back", rev.rev_id, rev.op)),
            };
            to.updated_at_ms = ts;
            all.insert(to.id, to.clone());
            steps.push((rev.rev_id, rev.op.clone(), after, to));
        }
        for note in all.values().filter(|n| n.status == NoteStatus::Active) {
            validate_parents_shape(note.kind, &note.parents).map_err(|e| e.to_string())?;
            Self::verify_parents_exist(&all, note.kind, &note.parents)?;
        }
        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            for (rev_id, op, from, to) in &steps {
                let new_id = Self::next_counter(&mut meta, "next_rev")?;
                if revs.get(new_id).map_err(|e| e.to_string())?.is_some() { return Err("revision counter collision".into()); }
                let bytes = serde_json::to_vec(to).map_err(|e| e.to_string())?;
                notes.insert(to.id, bytes.as_slice()).map_err(|e| e.to_string())?;
                let record = Revision { rev_id: new_id, ts_ms: ts, op: format!("rollback_{op}:{rev_id}"),
                    note_id: to.id, before: Some(from.clone()), after: Some(to.clone()) };
                let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
                revs.insert(new_id, bytes.as_slice()).map_err(|e| e.to_string())?;
            }
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(steps.into_iter().map(|(_, _, _, to)| to).collect())
    }

    pub fn supersede(&self, old_id: u64, draft: Draft) -> Result<Note, String> {
        let draft = draft.validate().map_err(|e| e.to_string())?;
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let existing = Self::load_all_notes_locked(&db)?;
        let mut old_note = existing
            .get(&old_id)
            .cloned()
            .ok_or_else(|| format!("note {old_id} not found"))?;
        if old_note.status != NoteStatus::Active { return Err("cannot supersede non-active note".into()); }
        if existing.len() >= self.max_notes { return Err(format!("store reached its note limit ({})", self.max_notes)); }
        if old_note.kind != Kind::Claim || draft.kind != Kind::Claim {
            return Err("supersede is restricted to claim notes".into());
        }
        Self::verify_parents_exist(&existing, draft.kind, &draft.parents)?;

        let ts = now_ms();
        let before_old = old_note.clone();
        old_note.status = NoteStatus::Superseded;
        old_note.updated_at_ms = ts;

        let w = db.begin_write().map_err(|e| e.to_string())?;
        let new_note = {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let new_id = Self::next_counter(&mut meta, "next_id")?;
            let rev_old = Self::next_counter(&mut meta, "next_rev")?;
            let rev_new = Self::next_counter(&mut meta, "next_rev")?;
            if notes.get(new_id).map_err(|e| e.to_string())?.is_some() || revs.get(rev_old).map_err(|e| e.to_string())?.is_some() || revs.get(rev_new).map_err(|e| e.to_string())?.is_some() { return Err("counter collision".into()); }

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
        let db = self.open_db()?;
        let all = Self::load_all_notes_locked(&db)?;
        let target = all
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("note {id} not found"))?;
        if target.status != NoteStatus::Active { return Err("cannot retract non-active note".into()); }
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

        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let mut revs = w.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            for (before, after) in &mutated {
                let rev_id = Self::next_counter(&mut meta, "next_rev")?;
                if revs.get(rev_id).map_err(|e| e.to_string())?.is_some() { return Err("revision counter collision".into()); }
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
        let all = self.load_all_notes()?;
        Self::lexical_ids(&all, query, limit)
    }

    fn lexical_ids(all: &BTreeMap<u64, Note>, query: &str, limit: usize) -> Result<Vec<u64>, String> {
        let q = validate_query(query).map_err(|e| e.to_string())?;
        let terms = tokenize(&q);
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

    /// The knowledge base version (`project_kb_version`): it changes with every note change
    /// (create, update, supersede, retract, rollback), so anything derived from the notes can
    /// record the version it was built at and be rebuilt only when this number moves.
    ///
    /// The fast path trusts the small `kb.version` file only while it is self-validating: it
    /// records the version with the length and modification time of `memory.redb` as they
    /// were when the last database handle was closed, still under the store lock (`StoreDb`
    /// does this for every handle `open_db` hands out, so every committing writer publishes
    /// in the same critical section as its commit). One `stat` of the database then shows
    /// whether anything touched it since, by this or any other writer: a writer that died
    /// before publishing, or an older build that never publishes, still changes the length
    /// or the time (opening and closing redb alone rewrites its header). Otherwise the
    /// database is opened, the counter read, and the file rewritten.
    pub fn kb_version(&self) -> Result<u64, String> {
        if let Some(v) = self.validated_version() {
            return Ok(v);
        }
        #[cfg(test)]
        SLOW_VERSION_CHECKS.with(|c| c.set(c.get() + 1));
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        Self::kb_version_locked(&db) // `db` publishes the version when it closes
    }

    fn version_path(&self) -> PathBuf {
        self.root.join("kb.version")
    }

    fn db_stamp(&self) -> Option<(u64, u128)> {
        let m = fs::metadata(self.db_path()).ok()?;
        let ns = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
        Some((m.len(), ns))
    }

    /// The recorded version, only if the file parses and its (length, mtime) equals the
    /// database's right now.
    fn validated_version(&self) -> Option<u64> {
        let stamp = self.db_stamp()?;
        let text = fs::read_to_string(self.version_path()).ok()?;
        let mut f = text.split_whitespace();
        if f.next()? != "v3" {
            return None;
        }
        let version: u64 = f.next()?.parse().ok()?;
        let len: u64 = f.next()?.parse().ok()?;
        let mtime: u128 = f.next()?.parse().ok()?;
        if f.next().is_some() || (len, mtime) != stamp {
            return None;
        }
        Some(version)
    }

    /// Writes `kb.version` for version `v`, stamping the database as it is now (all handles
    /// closed, store lock held). Any failure removes the file; if even that fails, the old
    /// record's stamp no longer matches the database, so it is never trusted.
    fn write_version_file(&self, v: u64) {
        let ok = (|| {
            let (len, mtime) = self.db_stamp()?;
            let tmp = self.root.join(format!("kb.version.{}.tmp", std::process::id()));
            if fs::write(&tmp, format!("v3 {v} {len} {mtime}")).is_err() || fs::rename(&tmp, self.version_path()).is_err() {
                let _ = fs::remove_file(&tmp);
                return None;
            }
            Some(())
        })();
        if ok.is_none() {
            let _ = fs::remove_file(self.version_path());
        }
    }

    fn kb_version_locked(db: &Database) -> Result<u64, String> {
        let r = db.begin_read().map_err(|e| e.to_string())?;
        let meta = r.open_table(META_TABLE).map_err(|e| e.to_string())?;
        let v = meta.get("next_rev").map_err(|e| e.to_string())?.ok_or("missing next_rev counter")?;
        Ok(u64::from_le_bytes(v.value().try_into().map_err(|_| "invalid next_rev counter".to_string())?))
    }

    /// Rebuilds the adjacency index when the knowledge base changed since it was built
    /// (one full read of the notes, on the first expansion after a change only).
    fn ensure_adjacency(db: &Database) -> Result<(), String> {
        let version = Self::kb_version_locked(db)?;
        {
            let r = db.begin_read().map_err(|e| e.to_string())?;
            let meta = r.open_table(META_TABLE).map_err(|e| e.to_string())?;
            let built = meta
                .get("adjacency_version")
                .map_err(|e| e.to_string())?
                .and_then(|v| <[u8; 8]>::try_from(v.value()).ok())
                .map(u64::from_le_bytes);
            if built == Some(version) {
                return Ok(());
            }
        }
        let all = Self::load_all_notes_locked(db)?;
        let mut claims: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for c in all.values().filter(|c| c.status == NoteStatus::Active && c.kind == Kind::Claim) {
            for &pid in &c.parents {
                if all.get(&pid).is_some_and(|p| p.status == NoteStatus::Active && p.kind == Kind::Knowledge) {
                    claims.entry(pid).or_default().push(c.id);
                }
            }
        }
        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            let _ = w.delete_table(ADJACENCY_TABLE);
            let mut adj = w.open_table(ADJACENCY_TABLE).map_err(|e| e.to_string())?;
            for (id, kids) in &claims {
                let bytes = serde_json::to_vec(kids).map_err(|e| e.to_string())?;
                adj.insert(*id, bytes.as_slice()).map_err(|e| e.to_string())?;
            }
            let mut meta = w.open_table(META_TABLE).map_err(|e| e.to_string())?;
            meta.insert("adjacency_version", version.to_le_bytes().as_slice()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())
    }

    /// The ids that are active notes right now, in the given order (point reads).
    pub fn filter_active(&self, ids: &[u64]) -> Result<Vec<u64>, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        let r = db.begin_read().map_err(|e| e.to_string())?;
        let notes = r.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for &id in ids {
            if let Some(v) = notes.get(id).map_err(|e| e.to_string())? {
                let n: Note = serde_json::from_slice(v.value()).map_err(|e| e.to_string())?;
                if n.status == NoteStatus::Active {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// One-hop expansion of retrieval seeds without reading every note: the seeds first, then
    /// for each seed a claim adds its active parent knowledge and a knowledge note adds up to
    /// `CLAIMS_PER_KNOWLEDGE` of its active claims, through the adjacency index and point reads.
    pub fn expand_one_hop(&self, seed_ids: &[u64], max_total: usize) -> Result<Vec<Note>, String> {
        let _lock = acquire_os_lock(&self.root)?;
        let db = self.open_db()?;
        Self::ensure_adjacency(&db)?;
        let r = db.begin_read().map_err(|e| e.to_string())?;
        let notes = r.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
        let adj = r.open_table(ADJACENCY_TABLE).map_err(|e| e.to_string())?;
        let mut memo: BTreeMap<u64, Option<Note>> = BTreeMap::new();
        let mut active = |id: u64| -> Result<Option<Note>, String> {
            if !memo.contains_key(&id) {
                let note = match notes.get(id).map_err(|e| e.to_string())? {
                    Some(v) => Some(serde_json::from_slice::<Note>(v.value()).map_err(|e| e.to_string())?),
                    None => None,
                };
                memo.insert(id, note.filter(|n| n.status == NoteStatus::Active));
            }
            Ok(memo[&id].clone())
        };
        let mut selected: Vec<u64> = Vec::new();
        for &id in seed_ids {
            if selected.len() >= max_total {
                break;
            }
            if !selected.contains(&id) && active(id)?.is_some() {
                selected.push(id);
            }
        }
        for &id in seed_ids {
            if selected.len() >= max_total {
                break;
            }
            let Some(n) = active(id)? else { continue };
            match n.kind {
                Kind::Claim => {
                    for &pid in &n.parents {
                        if selected.len() >= max_total {
                            break;
                        }
                        if !selected.contains(&pid) && active(pid)?.is_some() {
                            selected.push(pid);
                        }
                    }
                }
                Kind::Knowledge => {
                    let kids: Vec<u64> = match adj.get(id).map_err(|e| e.to_string())? {
                        Some(v) => serde_json::from_slice(v.value()).map_err(|e| e.to_string())?,
                        None => Vec::new(),
                    };
                    for cid in kids.into_iter().take(CLAIMS_PER_KNOWLEDGE) {
                        if selected.len() >= max_total {
                            break;
                        }
                        if !selected.contains(&cid) && active(cid)?.is_some() {
                            selected.push(cid);
                        }
                    }
                }
                Kind::Topic => {}
            }
        }
        let mut out = Vec::new();
        for id in selected {
            if let Some(n) = active(id)? {
                out.push(n);
            }
        }
        Ok(out)
    }

    fn expand_from(all: &BTreeMap<u64, Note>, seed_ids: &[u64], max_total: usize) -> Vec<Note> {
        let mut selected = Vec::new();
        for &id in seed_ids {
            if selected.len() >= max_total { break; }
            if all.get(&id).is_some_and(|n| n.status == NoteStatus::Active) && !selected.contains(&id) {
                selected.push(id);
            }
        }
        for &id in seed_ids {
            if selected.len() >= max_total {
                break;
            }
            if let Some(n) = all.get(&id).filter(|n| n.status == NoteStatus::Active) {
                match n.kind {
                    Kind::Claim => {
                        for &pid in &n.parents {
                            if selected.len() >= max_total { break; }
                            if all.get(&pid).is_some_and(|p| p.status == NoteStatus::Active) && !selected.contains(&pid) { selected.push(pid); }
                        }
                    }
                    Kind::Knowledge => {
                        for c in all.values().filter(|c| {
                            c.status == NoteStatus::Active
                                && c.kind == Kind::Claim
                                && c.parents.contains(&id)
                        }).take(CLAIMS_PER_KNOWLEDGE) {
                            if selected.len() >= max_total { break; }
                            if !selected.contains(&c.id) { selected.push(c.id); }
                        }
                    }
                    Kind::Topic => {}
                }
            }
        }
        selected.into_iter().filter_map(|id| all.get(&id).cloned()).collect()
    }

    pub fn search(&self, query: &str) -> Result<Vec<Note>, String> {
        let all = self.load_all_notes()?;
        let direct = Self::lexical_ids(&all, query, 5)?;
        Ok(Self::expand_from(&all, &direct, 8))
    }

    pub fn snapshot(&self, query: Option<&str>, after: Option<u64>) -> Result<Value, String> {
        let all = self.load_all_notes()?;
        if query.is_some_and(|q| q.len() > memb_core::MAX_QUERY_BYTES) { return Err("query exceeds 512 bytes".into()); }
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let p = std::env::temp_dir().join(format!("memb_test_{}_{}_{}_{}", tag, std::process::id(), now_ms(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            match fs::create_dir(&p) {
                Ok(()) => return p,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create test directory: {e}"),
            }
        }
    }

    /// Child half of the cross-process version tests. `MEMB_TEST_NO_PUBLISH` makes it behave
    /// like a writer that never writes `kb.version` (an older build, or one that died first).
    #[test]
    fn version_check_child_writer() {
        let Ok(dir) = std::env::var("MEMB_TEST_VERSION_CHILD") else { return };
        if std::env::var("MEMB_TEST_NO_PUBLISH").is_ok() {
            SKIP_PUBLISH.with(|c| c.set(true));
        }
        let store = Store::open(Path::new(&dir)).unwrap();
        store
            .remember(Draft { title: "from the child".into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
            .unwrap();
    }

    fn run_child(dir: &Path, no_publish: bool) {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "tests::version_check_child_writer", "--nocapture"]).env("MEMB_TEST_VERSION_CHILD", dir);
        if no_publish {
            cmd.env("MEMB_TEST_NO_PUBLISH", "1");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    fn slow_checks() -> usize {
        SLOW_VERSION_CHECKS.with(|c| c.get())
    }

    /// Checks the version, then that further calls are answered from the file (no database open).
    fn assert_fast_once_settled(store: &Store, expect: u64) {
        assert_eq!(store.kb_version().unwrap(), expect); // may repair the file (slow)
        let before = slow_checks();
        for _ in 0..5 {
            assert_eq!(store.kb_version().unwrap(), expect);
        }
        assert_eq!(slow_checks(), before, "an unchanged knowledge base is answered from the file");
    }

    #[test]
    fn a_note_change_from_another_process_is_seen_by_the_next_version_check() {
        let dir = temp_dir("xproc");
        let store = Store::open(&dir).unwrap();
        let v0 = store.kb_version().unwrap();
        assert_fast_once_settled(&store, v0);
        run_child(&dir, false);
        let v1 = store.kb_version().unwrap();
        assert!(v1 > v0, "the other process's note moved the version ({v0} -> {v1})");
        assert_eq!(store.load_all_notes().unwrap().len(), 1);
        assert_fast_once_settled(&store, v1);
        run_child(&dir, false);
        assert!(store.kb_version().unwrap() > v1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_writer_that_never_publishes_is_still_seen() {
        let dir = temp_dir("foreign");
        let store = Store::open(&dir).unwrap();
        let v0 = store.kb_version().unwrap();
        assert_fast_once_settled(&store, v0);
        let published = fs::read_to_string(dir.join("kb.version")).unwrap();
        run_child(&dir, true);
        assert_eq!(fs::read_to_string(dir.join("kb.version")).unwrap(), published, "the foreign writer left kb.version alone");
        let v1 = store.kb_version().unwrap();
        assert!(v1 > v0, "a commit by a writer that does not publish moved the version ({v0} -> {v1})");
        assert_fast_once_settled(&store, v1);
        // And again, with the file already settled at the new value.
        run_child(&dir, true);
        assert!(store.kb_version().unwrap() > v1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_writer_that_dies_between_commit_and_publish_is_seen_and_the_file_is_repaired() {
        let dir = temp_dir("crash");
        let store = Store::open(&dir).unwrap();
        let v0 = store.kb_version().unwrap();
        assert_fast_once_settled(&store, v0);
        SKIP_PUBLISH.with(|c| c.set(true)); // the commit happens, the publish does not
        let note = store
            .remember(Draft { title: "committed, never published".into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
            .unwrap();
        SKIP_PUBLISH.with(|c| c.set(false));
        assert!(fs::read_to_string(dir.join("kb.version")).unwrap().starts_with(&format!("v3 {v0} ")), "still the old record");
        let v1 = store.kb_version().unwrap();
        assert!(v1 > v0, "the committed change is visible ({v0} -> {v1}, note {})", note.id);
        assert_fast_once_settled(&store, v1);
        assert!(fs::read_to_string(dir.join("kb.version")).unwrap().starts_with(&format!("v3 {v1} ")), "the file was repaired");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_garbled_or_stale_version_file_falls_back_to_the_database() {
        let dir = temp_dir("garbled");
        let store = Store::open(&dir).unwrap();
        store
            .remember(Draft { title: "n".into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
            .unwrap();
        let v = store.kb_version().unwrap();
        for bad in ["", "garbage", "0", "v3 0 0 0 0", "v3 999 1 2 3", "v3 7 notanumber 1 2", "v3 1 2 3 4 5"] {
            fs::write(dir.join("kb.version"), bad).unwrap();
            assert_eq!(store.kb_version().unwrap(), v, "file {bad:?}");
        }
        fs::remove_file(dir.join("kb.version")).unwrap();
        assert_eq!(store.kb_version().unwrap(), v, "a missing file");
        assert!(fs::read_to_string(dir.join("kb.version")).unwrap().starts_with(&format!("v3 {v} ")), "is rewritten");
        // A record that validates against a database that has since changed is never trusted:
        // keep an old record, change the database, put the record back.
        store.kb_version().unwrap();
        let old = fs::read_to_string(dir.join("kb.version")).unwrap();
        store
            .remember(Draft { title: "m".into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
            .unwrap();
        fs::write(dir.join("kb.version"), old).unwrap();
        assert!(store.kb_version().unwrap() > v, "an old record does not validate against a newer database");
        fs::remove_dir_all(dir).unwrap();
    }

    /// Release-build breakdown of the version check: `cargo test -p memb-store --release -- --ignored --nocapture version_check_cost`
    #[test]
    #[ignore]
    fn version_check_cost() {
        let dir = temp_dir("cost");
        let store = Store::open(&dir).unwrap();
        for i in 0..500 {
            store
                .remember(Draft { title: format!("note {i}"), desc: "some text about the thing ".repeat(8), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() })
                .unwrap();
        }
        let n = 50u32;
        let time = |label: &str, f: &dyn Fn()| {
            f();
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            println!("{label}: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        };
        time("os lock only", &|| drop(acquire_os_lock(&dir).unwrap()));
        time("open redb (no legacy check)", &|| drop(Database::builder().set_cache_size(16 * 1024 * 1024).create(store.db_path()).unwrap()));
        let db = store.open_db().unwrap();
        time("legacy verification (open handle)", &|| Store::verify_legacy(&db).unwrap());
        time("read next_rev (open handle)", &|| { let _ = Store::kb_version_locked(&db).unwrap(); });
        drop(db);
        time("slow-path kb_version (lock + open + legacy + read + publish on close)", &|| {
            let _lock = acquire_os_lock(&dir).unwrap();
            let db = store.open_db().unwrap();
            Store::kb_version_locked(&db).unwrap();
        });
        store.kb_version().unwrap();
        time("fast-path kb_version (stat + version file)", &|| { let _ = store.kb_version().unwrap(); });
        time("filter_active, one id (one open, publishes on close)", &|| { let _ = store.filter_active(&[1]).unwrap(); });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn revisions_list_newest_first_filter_by_note_and_respect_the_limit() {
        let dir = temp_dir("revlist");
        let store = Store::open(&dir).unwrap();
        assert!(store.list_revisions(None, 10).unwrap().is_empty());
        let topic = |title: &str| Draft { title: title.into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() };
        let a = store.remember(topic("A")).unwrap();
        let b = store.remember(topic("B")).unwrap();
        store.update(a.id, NotePatch { desc: Some("edited".into()), ..Default::default() }).unwrap();
        let all = store.list_revisions(None, 10).unwrap();
        assert_eq!(all.iter().map(|r| r.rev_id).collect::<Vec<_>>(), vec![3, 2, 1]);
        assert_eq!(all[0].op, "update");
        assert_eq!(store.list_revisions(Some(a.id), 10).unwrap().iter().map(|r| r.rev_id).collect::<Vec<_>>(), vec![3, 1]);
        assert_eq!(store.list_revisions(Some(b.id), 10).unwrap().len(), 1);
        assert_eq!(store.list_revisions(None, 2).unwrap().len(), 2);
        assert_eq!(store.list_revisions(Some(999), 10).unwrap().len(), 0);
        assert_eq!(store.list_revisions(None, 0).unwrap().len(), 1); // limit is clamped to at least one
    }

    #[test]
    fn configured_note_limit_applies_to_remember_and_supersede_but_never_exceeds_the_hard_cap() {
        let dir = temp_dir("limit");
        let store = Store::open(&dir).unwrap().with_max_notes(2);
        let topic = |title: &str| Draft { title: title.into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() };
        let a = store.remember(topic("A")).unwrap();
        store.remember(topic("B")).unwrap();
        assert!(store.remember(topic("C")).unwrap_err().contains("note limit (2)"));
        assert!(store.supersede(a.id, topic("A2")).unwrap_err().contains("note limit (2)"));
        // Existing notes stay readable and editable at the limit.
        assert!(store.update(a.id, NotePatch { desc: Some("edited".into()), ..Default::default() }).is_ok());
        // A larger configured value is clamped to the built-in cap; zero is clamped to one.
        assert_eq!(Store::open(&dir).unwrap().with_max_notes(usize::MAX).max_notes, MAX_NOTES);
        assert_eq!(Store::open(&dir).unwrap().with_max_notes(0).max_notes, 1);
    }

    #[test]
    fn expansion_uses_the_adjacency_index_caps_claims_and_rebuilds_only_after_a_change() {
        let dir = temp_dir("adjacency");
        let store = Store::open(&dir).unwrap();
        let note = |title: &str, kind: Kind, parents: Vec<u64>| Draft { title: title.into(), desc: "text".into(), kind, parents, properties: BTreeMap::new() };
        let topic = store.remember(note("T", Kind::Topic, vec![])).unwrap();
        let k = store.remember(note("K", Kind::Knowledge, vec![topic.id])).unwrap();
        let other = store.remember(note("K2", Kind::Knowledge, vec![topic.id])).unwrap();
        let claims: Vec<u64> = (0..6)
            .map(|i| store.remember(note(&format!("c{i}"), Kind::Claim, vec![k.id])).unwrap().id)
            .collect();
        let version = store.kb_version().unwrap();
        // The first expansion builds the index (one full read); later ones read no more than the few notes they need.
        let before = full_scan_count();
        let hop = store.expand_one_hop(&[k.id], 8).unwrap();
        assert_eq!(full_scan_count() - before, 1, "index built once");
        let ids: Vec<u64> = hop.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![k.id, claims[0], claims[1], claims[2], claims[3]], "the seed, then at most {CLAIMS_PER_KNOWLEDGE} claims in id order");
        let before = full_scan_count();
        for _ in 0..50 {
            assert_eq!(store.expand_one_hop(&[k.id], 8).unwrap().len(), 1 + CLAIMS_PER_KNOWLEDGE);
        }
        // A claim brings its parent knowledge; a seed that is not active, or does not exist, brings nothing.
        assert_eq!(store.expand_one_hop(&[claims[5]], 8).unwrap().iter().map(|n| n.id).collect::<Vec<_>>(), vec![claims[5], k.id]);
        assert!(store.expand_one_hop(&[9999], 8).unwrap().is_empty());
        assert_eq!(store.filter_active(&[claims[5], 9999, k.id]).unwrap(), vec![claims[5], k.id]);
        assert_eq!(store.expand_one_hop(&[other.id], 8).unwrap().len(), 1);
        assert_eq!(full_scan_count(), before, "no expansion read every note");
        assert_eq!(store.kb_version().unwrap(), version, "reads do not change the version");
        // A change moves the version; the next expansion rebuilds once and sees it.
        store.retract(claims[0], None, false).unwrap();
        assert!(store.kb_version().unwrap() > version);
        let before = full_scan_count(); // the retraction itself read the notes
        let ids: Vec<u64> = store.expand_one_hop(&[k.id], 8).unwrap().iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![k.id, claims[1], claims[2], claims[3], claims[4]]);
        assert_eq!(full_scan_count() - before, 1, "rebuilt once after the change");
        store.expand_one_hop(&[k.id], 8).unwrap();
        assert_eq!(full_scan_count() - before, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rollback_update_checks_conflicts_dependencies_and_records_revision() {
        let dir = temp_dir("rollback");
        let store = Store::open(&dir).unwrap();
        let topic = |title: &str| Draft { title: title.into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() };
        let a = store.remember(topic("A")).unwrap(); // revision 1
        let b = store.remember(topic("B")).unwrap(); // revision 2
        let k = store.remember(Draft { title: "K".into(), desc: "text".into(), kind: Kind::Knowledge, parents: vec![a.id], properties: BTreeMap::new() }).unwrap(); // 3
        let updated = store.update(k.id, NotePatch { parents: Some(vec![b.id]), ..Default::default() }).unwrap(); // 4
        let preview = store.revision(4).unwrap().unwrap();
        assert_eq!(preview.before.as_ref().unwrap().parents, vec![a.id]);
        assert_eq!(preview.after.as_ref(), Some(&updated));
        assert!(store.revision(999).unwrap().is_none());
        assert_eq!(store.inspect(k.id).unwrap().unwrap(), updated);
        let restored = store.rollback_revision(4).unwrap(); // 5
        assert_eq!(restored.parents, vec![a.id]);
        assert!(store.rollback_revision(4).unwrap_err().contains("conflict"));
        store.update(k.id, NotePatch { parents: Some(vec![b.id]), ..Default::default() }).unwrap(); // 6
        store.retract(a.id, None, false).unwrap(); // 7
        assert!(store.rollback_revision(6).is_err());
        assert_eq!(store.inspect(k.id).unwrap().unwrap().parents, vec![b.id]);
        assert!(store.rollback_revision(7).unwrap_err().contains("unsupported"));
        store.with_db(|db| {
            let r = db.begin_read().map_err(|e| e.to_string())?;
            let t = r.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
            let v = t.get(5).map_err(|e| e.to_string())?.unwrap();
            let rev: Revision = serde_json::from_slice(v.value()).map_err(|e| e.to_string())?;
            assert_eq!(rev.op, "rollback_update:4");
            Ok(())
        }).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grouped_rollback_reverses_creations_supersessions_and_retractions() {
        let dir = temp_dir("group_rollback");
        let store = Store::open(&dir).unwrap();
        let d = |title: &str, kind: Kind, parents: Vec<u64>| Draft { title: title.into(), desc: "text".into(), kind, parents, properties: BTreeMap::new() };
        let t = store.remember(d("T", Kind::Topic, vec![])).unwrap();
        let k = store.remember(d("K", Kind::Knowledge, vec![t.id])).unwrap();
        let c = store.remember(d("C", Kind::Claim, vec![k.id])).unwrap();
        let mark = store.latest_revision_id().unwrap();
        // One "curation edit": supersede the claim.
        let c2 = store.supersede(c.id, d("C2", Kind::Claim, vec![k.id])).unwrap();
        let group: Vec<u64> = (mark + 1..=store.latest_revision_id().unwrap()).collect();
        assert_eq!(group.len(), 2);
        let back = store.rollback_revisions(&group).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(store.inspect(c.id).unwrap().unwrap().status, NoteStatus::Active);
        assert_eq!(store.inspect(c2.id).unwrap().unwrap().status, NoteStatus::Retracted);
        assert!(store.rollback_revisions(&group).unwrap_err().contains("conflict"), "a group cannot be undone twice");
        // A retraction is undone.
        let mark = store.latest_revision_id().unwrap();
        store.retract(c.id, None, false).unwrap();
        let group: Vec<u64> = (mark + 1..=store.latest_revision_id().unwrap()).collect();
        store.rollback_revisions(&group).unwrap();
        assert_eq!(store.inspect(c.id).unwrap().unwrap().status, NoteStatus::Active);
        // A creation is undone, but not while something active hangs under it.
        let mark = store.latest_revision_id().unwrap();
        let k2 = store.remember(d("K2", Kind::Knowledge, vec![t.id])).unwrap();
        let kid = (mark + 1..=store.latest_revision_id().unwrap()).collect::<Vec<_>>();
        let child = store.remember(d("C3", Kind::Claim, vec![k2.id])).unwrap();
        assert!(store.rollback_revisions(&kid).is_err());
        assert_eq!(store.inspect(k2.id).unwrap().unwrap().status, NoteStatus::Active);
        store.retract(child.id, None, false).unwrap();
        // The claim was edited later (retracted), so the creation of K2 can go but a stale group cannot.
        store.rollback_revisions(&kid).unwrap();
        assert_eq!(store.inspect(k2.id).unwrap().unwrap().status, NoteStatus::Retracted);
        // Rollback records are not themselves rollbackable, and unknown ids fail without changes.
        let last = store.latest_revision_id().unwrap();
        assert!(store.rollback_revisions(&[last]).unwrap_err().contains("cannot be rolled back"));
        assert!(store.rollback_revisions(&[last + 5]).is_err());
        assert!(store.rollback_revisions(&[]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn with_db_keeps_extension_data_and_detects_legacy_changes() {
        const EXT: TableDefinition<&str, &[u8]> = TableDefinition::new("synthetic_extension");
        let dir = temp_dir("with_db");
        let store = Store::open(&dir).unwrap();
        store.with_db(|db| {
            let w = db.begin_write().map_err(|e| e.to_string())?;
            { let mut t = w.open_table(EXT).map_err(|e| e.to_string())?;
              t.insert("key", b"value".as_slice()).map_err(|e| e.to_string())?; }
            w.commit().map_err(|e| e.to_string())
        }).unwrap();
        let value = store.with_db(|db| {
            let r = db.begin_read().map_err(|e| e.to_string())?;
            let t = r.open_table(EXT).map_err(|e| e.to_string())?;
            let value = t.get("key").map_err(|e| e.to_string())?.unwrap().value().to_vec();
            Ok(value)
        }).unwrap();
        assert_eq!(value, b"value");
        let err = store.with_db(|db| {
            let w = db.begin_write().map_err(|e| e.to_string())?;
            { let mut t = w.open_table(LEGACY_PROJECTS_TABLE).map_err(|e| e.to_string())?;
              t.insert("unexpected", b"{}".as_slice()).map_err(|e| e.to_string())?; }
            w.commit().map_err(|e| e.to_string())
        }).unwrap_err();
        assert!(err.contains("legacy projects changed"));
        assert!(store.load_all_notes().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expansion_excludes_inactive_seeds_and_parents_and_obeys_limit() {
        let dir = temp_dir("active_search");
        let store = Store::open(&dir).unwrap();
        let topic = store.remember(Draft { title: "topic".into(), desc: "searchword".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() }).unwrap();
        let knowledge = store.remember(Draft { title: "knowledge".into(), desc: "searchword".into(), kind: Kind::Knowledge, parents: vec![topic.id], properties: BTreeMap::new() }).unwrap();
        let claim = store.remember(Draft { title: "claim".into(), desc: "searchword".into(), kind: Kind::Claim, parents: vec![knowledge.id], properties: BTreeMap::new() }).unwrap();
        store.retract(claim.id, None, false).unwrap();
        assert!(store.expand_one_hop(&[claim.id], 8).unwrap().is_empty());
        assert!(store.expand_one_hop(&[topic.id, knowledge.id], 1).unwrap().len() == 1);
        assert!(store.search("searchword").unwrap().iter().all(|n| n.status == NoteStatus::Active));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn separate_handles_and_legacy_mutation_fail_closed() {
        let dir = temp_dir("legacy_change");
        let a = Store::open(&dir).unwrap();
        let b = Store::open(&dir).unwrap();
        let draft = Draft { title: "topic".into(), desc: "text".into(), kind: Kind::Topic, parents: vec![], properties: BTreeMap::new() };
        let first = a.remember(draft.clone()).unwrap();
        let second = b.remember(draft).unwrap();
        assert_ne!(first.id, second.id);
        {
            let db = Database::open(dir.join("memory.redb")).unwrap();
            let w = db.begin_write().unwrap();
            { let mut table = w.open_table(LEGACY_PROJECTS_TABLE).unwrap();
              table.insert("late", br#"{\"next\":0,\"notes\":{}}"#.as_slice()).unwrap(); }
            w.commit().unwrap();
        }
        assert!(a.load_all_notes().unwrap_err().contains("legacy projects changed"));
        assert!(Store::open(&dir).err().unwrap().contains("legacy projects changed"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_refuses_existing_v2_data() {
        let dir = temp_dir("v2_collision");
        {
            let db = Database::create(dir.join("memory.redb")).unwrap();
            let w = db.begin_write().unwrap();
            { let mut table = w.open_table(NOTES_TABLE).unwrap();
              table.insert(1, b"existing".as_slice()).unwrap(); }
            w.commit().unwrap();
        }
        assert!(Store::open(&dir).err().unwrap().contains("existing v2 data"));
        let db = Database::open(dir.join("memory.redb")).unwrap();
        let r = db.begin_read().unwrap();
        let table = r.open_table(NOTES_TABLE).unwrap();
        assert_eq!(table.get(1).unwrap().unwrap().value(), b"existing");
        drop(table); drop(r); drop(db);
        fs::remove_dir_all(dir).unwrap();
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

        let err = Store::open(&dir).err().expect("must fail closed on corrupt legacy project");
        assert!(err.contains("aborting v1->v2 migration"));
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    use memb_core::{Draft, Kind};

    #[test]
    fn compaction_keeps_every_note_and_revision_and_never_grows_the_file() {
        let dir = std::env::temp_dir().join(format!("memb-compact-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut ids = Vec::new();
        for i in 0..200 {
            let note = store
                .remember(Draft {
                    title: format!("note {i}"),
                    desc: "x".repeat(2000),
                    kind: Kind::Topic,
                    parents: vec![],
                    properties: Default::default(),
                })
                .unwrap();
            ids.push(note.id);
        }
        for id in &ids[..150] {
            store.retract(*id, None, false).unwrap();
        }
        let notes_before = store.load_all_notes().unwrap();
        let revisions_before = store.list_revisions(None, 200).unwrap().len();
        let (before, after) = store.compact().unwrap();
        assert!(after <= before, "{after} > {before}");
        assert_eq!(store.load_all_notes().unwrap(), notes_before, "notes are untouched");
        assert_eq!(store.list_revisions(None, 200).unwrap().len(), revisions_before);
        // A second pass has nothing left to give back and still works.
        let (again_before, again_after) = store.compact().unwrap();
        assert!(again_after <= again_before);
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }
}
