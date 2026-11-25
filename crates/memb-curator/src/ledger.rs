//! Durable list of curation proposals, shared by every process that opens the store.
//!
//! A proposal is only a record of something the model suggested. Nothing here changes
//! a note: applying one needs the user's explicit approval of that exact proposal, and
//! the record can be claimed once. Records older than the staging limit expire.
//!
//! All functions take the open database and are meant to run inside
//! `Store::with_db`, which serializes them with every other store operation.
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROPOSALS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("curator_proposals_v1");
/// Pending proposals kept at once; a new one is refused beyond this.
pub const MAX_PENDING: usize = 200;
const MAX_ACTION_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting for the user's decision.
    Pending,
    /// Claimed for applying. If the process dies here the user can reject it; it is
    /// never applied a second time.
    Applying,
    Applied,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProposal {
    pub id: u64,
    pub status: Status,
    pub created_ms: u64,
    pub decided_ms: Option<u64>,
    /// The action exactly as proposed (without `source_chunks`).
    pub action: Value,
    /// Verified `(session_id, chunk_id)` pairs the host attached.
    pub sources: Vec<(String, String)>,
    /// The note the applied action created or changed.
    pub note_id: Option<u64>,
}

fn load(raw: &[u8]) -> Result<StoredProposal, String> {
    serde_json::from_slice(raw).map_err(|_| "stored proposal is unreadable".to_string())
}

fn put(table: &mut redb::Table<u64, &[u8]>, p: &StoredProposal) -> Result<(), String> {
    let bytes = serde_json::to_vec(p).map_err(|e| e.to_string())?;
    table
        .insert(p.id, bytes.as_slice())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Records a new pending proposal and returns its id (one more than the highest used).
pub fn add(
    db: &Database,
    now_ms: u64,
    action: Value,
    sources: Vec<(String, String)>,
) -> Result<u64, String> {
    if serde_json::to_vec(&action).map_err(|e| e.to_string())?.len() > MAX_ACTION_BYTES {
        return Err("proposal is too large".into());
    }
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let id = {
        let mut table = w.open_table(PROPOSALS_TABLE).map_err(|e| e.to_string())?;
        let mut highest = 0;
        let mut pending = 0;
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            highest = highest.max(key.value());
            if load(value.value())?.status == Status::Pending {
                pending += 1;
            }
        }
        if pending >= MAX_PENDING {
            return Err("too many pending proposals; apply or reject some first".into());
        }
        let id = highest.checked_add(1).ok_or("proposal counter exhausted")?;
        put(
            &mut table,
            &StoredProposal {
                id,
                status: Status::Pending,
                created_ms: now_ms,
                decided_ms: None,
                action,
                sources,
                note_id: None,
            },
        )?;
        id
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(id)
}

/// Newest first, optionally only one status, at most `limit` (1..=200).
pub fn list(
    db: &Database,
    status: Option<&Status>,
    limit: usize,
) -> Result<Vec<StoredProposal>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(PROPOSALS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = Vec::new();
    for entry in table.iter().map_err(|e| e.to_string())?.rev() {
        let (_, value) = entry.map_err(|e| e.to_string())?;
        let p = load(value.value())?;
        if status.is_none_or(|s| *s == p.status) {
            out.push(p);
            if out.len() >= limit.clamp(1, 200) {
                break;
            }
        }
    }
    Ok(out)
}

pub fn get(db: &Database, id: u64) -> Result<Option<StoredProposal>, String> {
    let r = db.begin_read().map_err(|e| e.to_string())?;
    let table = match r.open_table(PROPOSALS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let found = table.get(id).map_err(|e| e.to_string())?;
    found.map(|v| load(v.value())).transpose()
}

fn transition(
    db: &Database,
    id: u64,
    now_ms: u64,
    from: &[Status],
    to: Status,
    note_id: Option<u64>,
) -> Result<StoredProposal, String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let updated = {
        let mut table = w.open_table(PROPOSALS_TABLE).map_err(|e| e.to_string())?;
        let current = {
            let raw = table.get(id).map_err(|e| e.to_string())?;
            let raw = raw.ok_or("proposal not found")?;
            load(raw.value())?
        };
        if !from.contains(&current.status) {
            return Err("proposal is not in a state that allows this".into());
        }
        let updated = StoredProposal {
            status: to,
            decided_ms: Some(now_ms),
            note_id: note_id.or(current.note_id),
            ..current
        };
        put(&mut table, &updated)?;
        updated
    };
    w.commit().map_err(|e| e.to_string())?;
    Ok(updated)
}

/// Pending to applying. Fails if the proposal is not pending (already used, rejected
/// or expired), which is what makes approval single-use across processes.
pub fn claim(db: &Database, id: u64, now_ms: u64) -> Result<StoredProposal, String> {
    transition(db, id, now_ms, &[Status::Pending], Status::Applying, None)
}

/// Applying to applied, recording the note that was created or changed.
pub fn finish(db: &Database, id: u64, now_ms: u64, note_id: Option<u64>) -> Result<(), String> {
    transition(db, id, now_ms, &[Status::Applying], Status::Applied, note_id).map(|_| ())
}

/// Pending (or a stuck applying) to rejected.
pub fn reject(db: &Database, id: u64, now_ms: u64) -> Result<(), String> {
    transition(
        db,
        id,
        now_ms,
        &[Status::Pending, Status::Applying],
        Status::Rejected,
        None,
    )
    .map(|_| ())
}

/// Pending proposals older than `ttl_ms` become expired. Returns how many.
pub fn expire(db: &Database, now_ms: u64, ttl_ms: u64) -> Result<usize, String> {
    let w = db.begin_write().map_err(|e| e.to_string())?;
    let mut expired = 0;
    {
        let mut table = w.open_table(PROPOSALS_TABLE).map_err(|e| e.to_string())?;
        let mut stale = Vec::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let p = load(value.value())?;
            if p.status == Status::Pending && now_ms.saturating_sub(p.created_ms) > ttl_ms {
                stale.push(p);
            }
        }
        for mut p in stale {
            p.status = Status::Expired;
            p.decided_ms = Some(now_ms);
            put(&mut table, &p)?;
            expired += 1;
        }
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(expired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> (Database, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "memb_ledger_{}_{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Database::create(&p).unwrap(), p)
    }

    #[test]
    fn proposals_are_single_use_and_expire() {
        let (db, path) = db();
        let src = vec![("s".to_string(), "c".to_string())];
        let a = add(&db, 1_000, json!({"action":"DEFER"}), src.clone()).unwrap();
        let b = add(&db, 2_000, json!({"action":"DEFER"}), src.clone()).unwrap();
        assert_eq!((a, b), (1, 2));
        assert_eq!(list(&db, None, 10).unwrap()[0].id, 2);
        assert_eq!(list(&db, Some(&Status::Pending), 10).unwrap().len(), 2);

        claim(&db, a, 3_000).unwrap();
        assert!(claim(&db, a, 3_001).is_err(), "a claimed proposal cannot be claimed again");
        assert!(finish(&db, b, 3_002, Some(9)).is_err(), "only a claimed proposal can finish");
        finish(&db, a, 3_003, Some(9)).unwrap();
        let done = get(&db, a).unwrap().unwrap();
        assert_eq!((done.status, done.note_id), (Status::Applied, Some(9)));
        assert!(claim(&db, a, 3_004).is_err());
        assert!(reject(&db, a, 3_005).is_err(), "an applied proposal cannot be rejected");

        // b is 29 days old: kept; at 31 days: expired and no longer claimable.
        let day = 24 * 60 * 60 * 1000;
        assert_eq!(expire(&db, 2_000 + 29 * day, 30 * day).unwrap(), 0);
        assert_eq!(expire(&db, 2_000 + 31 * day, 30 * day).unwrap(), 1);
        assert_eq!(get(&db, b).unwrap().unwrap().status, Status::Expired);
        assert!(claim(&db, b, 2_000 + 32 * day).is_err());
        assert!(get(&db, 99).unwrap().is_none());
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_stuck_claim_can_be_rejected_and_the_pending_limit_holds() {
        let (db, path) = db();
        let id = add(&db, 1, json!({"action":"DEFER"}), vec![]).unwrap();
        claim(&db, id, 2).unwrap();
        reject(&db, id, 3).unwrap();
        for _ in 0..MAX_PENDING {
            add(&db, 4, json!({"action":"DEFER"}), vec![]).unwrap();
        }
        assert!(add(&db, 5, json!({"action":"DEFER"}), vec![]).is_err());
        assert!(add(&db, 5, json!({"pad": "x".repeat(MAX_ACTION_BYTES)}), vec![]).is_err());
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
