//! Transactional local memory. The OS lock is released on process exit.

use memb_core::{words, Draft, Kind, Note, Scope};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;
const PROJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("projects_v1");
const MAX_NOTES: usize = 500;
const MAX_PROJECTS: u64 = 128;

#[derive(Default, Serialize, Deserialize)]
struct Project {
    next: u64,
    notes: BTreeMap<u64, Note>,
}

pub struct Store(PathBuf);
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            return Err("data directory must be absolute".into());
        }
        Ok(Self(root.to_owned()))
    }

    fn access<T>(
        &self,
        scope: &Scope,
        write: bool,
        f: impl FnOnce(&mut Project) -> Result<T>,
    ) -> Result<T> {
        let path = self.0.join("memory.redb");
        if !write && !path.try_exists()? {
            return f(&mut Project::default());
        }
        fs::create_dir_all(&self.0)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.0.join("store.lock"))?;
        // Windows file locking requires a file open for writing; keep the lock file stable.
        let start = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if start.elapsed() < Duration::from_secs(3) =>
                {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(
                        io::Error::new(io::ErrorKind::TimedOut, "memory store is busy").into(),
                    )
                }
                Err(std::fs::TryLockError::Error(err)) => return Err(err.into()),
            }
        }
        if !write && !path.try_exists()? {
            return f(&mut Project::default());
        }
        let db = Database::builder()
            .set_cache_size(16 * 1024 * 1024)
            .create(path)?;
        if write {
            let txn = db.begin_write()?;
            let output = {
                let mut table = txn.open_table(PROJECTS)?;
                let old = table.get(scope.as_str())?;
                if old.is_none() && table.len()? >= MAX_PROJECTS {
                    return Err("project limit reached".into());
                }
                let mut project = old
                    .map(|v| serde_json::from_slice(v.value()))
                    .transpose()?
                    .unwrap_or_default();
                let output = f(&mut project)?;
                if project.next > 0 {
                    let bytes = serde_json::to_vec(&project)?;
                    table.insert(scope.as_str(), bytes.as_slice())?;
                }
                output
            };
            txn.commit()?;
            Ok(output)
        } else {
            let txn = db.begin_read()?;
            let table = match txn.open_table(PROJECTS) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return f(&mut Project::default()),
                Err(err) => return Err(err.into()),
            };
            let mut project = table
                .get(scope.as_str())?
                .map(|v| serde_json::from_slice(v.value()))
                .transpose()?
                .unwrap_or_default();
            f(&mut project)
        }
    }

    pub fn remember(&self, scope: &Scope, draft: Draft) -> Result<Note> {
        let mut note = draft.into_note(0)?;
        self.access(scope, true, |project| {
            if project.notes.len() >= MAX_NOTES {
                return Err("note limit reached; forget an unused note first".into());
            }
            let parent_kind = match note.kind {
                Kind::Topic => None,
                Kind::Knowledge => Some(Kind::Topic),
                Kind::Claim => Some(Kind::Knowledge),
            };
            if note
                .parents
                .iter()
                .any(|p| project.notes.get(p).map(|n| n.kind) != parent_kind)
            {
                return Err(
                    "each parent must exist in this project and have the required type".into(),
                );
            }
            project.next = project.next.checked_add(1).ok_or("note ID limit reached")?;
            note.id = project.next;
            project.notes.insert(note.id, note.clone());
            Ok(note)
        })
    }

    pub fn inspect(&self, scope: &Scope, id: u64) -> Result<Option<Note>> {
        self.access(scope, false, |p| Ok(p.notes.get(&id).cloned()))
    }

    pub fn forget(&self, scope: &Scope, id: u64) -> Result<bool> {
        if !self.0.join("memory.redb").try_exists()? {
            return Ok(false);
        }
        self.access(scope, true, |p| {
            if p.notes.values().any(|n| n.parents.contains(&id)) {
                return Err("forget or rehome dependent notes first".into());
            }
            Ok(p.notes.remove(&id).is_some())
        })
    }

    pub fn search(&self, scope: &Scope, query: &str) -> Result<Vec<Note>> {
        if query.len() > 512 {
            return Err("query exceeds 512 bytes".into());
        }
        let terms: BTreeSet<_> = words(query).collect();
        if terms.is_empty() {
            return Err("query must include searchable text".into());
        }
        self.access(scope, false, |p| {
            let mut ranked: Vec<_> = p
                .notes
                .values()
                .filter_map(|note| {
                    let title: BTreeSet<_> = words(&note.title).collect();
                    let body: BTreeSet<_> = words(&note.body).collect();
                    let score =
                        3 * terms.intersection(&title).count() + terms.intersection(&body).count();
                    (score > 0).then_some((std::cmp::Reverse(score), note.id))
                })
                .collect();
            ranked.sort_unstable();
            let mut ids: Vec<_> = ranked.into_iter().take(5).map(|(_, id)| id).collect();
            let direct = ids.clone();
            for id in direct {
                let note = &p.notes[&id];
                if note.kind == Kind::Claim {
                    for parent in &note.parents {
                        if ids.len() < 8 && !ids.contains(parent) {
                            ids.push(*parent);
                        }
                    }
                } else if note.kind == Kind::Knowledge {
                    for claim in p
                        .notes
                        .values()
                        .filter(|n| n.kind == Kind::Claim && n.parents.contains(&id))
                    {
                        if ids.len() < 8 && !ids.contains(&claim.id) {
                            ids.push(claim.id);
                        }
                    }
                }
            }
            Ok(ids.into_iter().map(|id| p.notes[&id].clone()).collect())
        })
    }
}
