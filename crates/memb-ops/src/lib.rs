//! Conservative, unwired Phase 4 operations.
//!
//! The scheduler is a decision only: callers must persist successful runs and
//! provide their own wake delay, governor and pipeline. Rollback planning does
//! not mutate the store: atomic compare-and-restore, revision allocation, graph
//! validation and grouped multi-note revisions require a store-owned API.
//! Mirror output is create-only; it neither updates nor removes existing files,
//! imports edits, nor guarantees an atomic multi-file snapshot.

use memb_core::{Kind, Note, NoteStatus};
use memb_store::{Revision, Store};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

pub const MIN_COOLDOWN_MS: u64 = 20 * 3600 * 1000;
pub const OVERDUE_MS: u64 = 24 * 3600 * 1000;
pub const IDLE_SWEEP_AGE_MS: u64 = 24 * 3600 * 1000;
pub const IDLE_SWEEP_MIN_MESSAGES: usize = 45;

/// Evaluate the phase-4 policy using caller-supplied WIB time and last success.
/// Invalid clock fields and backwards wall-clock movement fail closed.
pub fn should_run_daily_routine(now_ms: u64, last_run_ms: u64, hour: u8, minute: u8) -> bool {
    should_run_daily_routine_with(&RoutinePolicy::default(), now_ms, last_run_ms, hour, minute)
}

/// When the daily routine is due (config.toml `[scheduler]`). `Default` is 15:45 WIB,
/// a 20-hour cooldown and a 24-hour catch-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutinePolicy {
    pub hour: u8,
    pub minute: u8,
    pub cooldown_ms: u64,
    pub overdue_ms: u64,
}

impl Default for RoutinePolicy {
    fn default() -> Self {
        Self { hour: 15, minute: 45, cooldown_ms: MIN_COOLDOWN_MS, overdue_ms: OVERDUE_MS }
    }
}

impl RoutinePolicy {
    /// The catch-up window is the larger of 24 hours and the cooldown, so a longer
    /// cooldown can never make a routine overdue before it is allowed to run.
    pub fn new(hour: u8, minute: u8, cooldown_hours: u64) -> Result<Self, String> {
        if hour > 23 || minute > 59 || !(1..=168).contains(&cooldown_hours) {
            return Err("invalid routine time or cooldown".into());
        }
        let cooldown_ms = cooldown_hours * 3600 * 1000;
        Ok(Self { hour, minute, cooldown_ms, overdue_ms: OVERDUE_MS.max(cooldown_ms) })
    }
}

/// `hour`/`minute` are the current WIB clock; the policy holds the target time.
pub fn should_run_daily_routine_with(policy: &RoutinePolicy, now_ms: u64, last_run_ms: u64, hour: u8, minute: u8) -> bool {
    if hour > 23 || minute > 59 || now_ms < last_run_ms { return false; }
    let elapsed = now_ms - last_run_ms;
    elapsed >= policy.cooldown_ms
        && (elapsed >= policy.overdue_ms || hour > policy.hour || (hour == policy.hour && minute >= policy.minute))
}

/// Maximum lease is bounded to limit how long a crashed worker blocks recovery.
pub const MAX_ROUTINE_LEASE_MS: u64 = 60 * 60 * 1000;
const ROUTINE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("ops_routine_v1");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineClaim { pub generation: u64, pub expires_at_ms: u64 }

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutineState {
    last_success_ms: u64,
    last_observed_ms: u64,
    generation: u64,
    claim: Option<RoutineClaim>,
}

/// Claim a due routine in an extension table. Call only under `Store::with_db`.
/// Expired workers are fenced from finishing, but callers must separately ensure
/// their pipeline cannot keep writing after expiry; a lease is not a process kill.
pub fn claim_daily_routine(db: &redb::Database, now_ms: u64, hour: u8, minute: u8, lease_ms: u64) -> Result<Option<RoutineClaim>, String> {
    claim_daily_routine_with(&RoutinePolicy::default(), db, now_ms, hour, minute, lease_ms)
}

pub fn claim_daily_routine_with(policy: &RoutinePolicy, db: &redb::Database, now_ms: u64, hour: u8, minute: u8, lease_ms: u64) -> Result<Option<RoutineClaim>, String> {
    if lease_ms == 0 || lease_ms > MAX_ROUTINE_LEASE_MS || hour > 23 || minute > 59 {
        return Err("invalid routine time or lease duration".into());
    }
    let expires_at_ms = now_ms.checked_add(lease_ms).ok_or("lease expiry overflow")?;
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let result = {
        let mut table = tx.open_table(ROUTINE_TABLE).map_err(|e| e.to_string())?;
        let mut state: RoutineState = match table.get("state").map_err(|e| e.to_string())? {
            Some(v) => serde_json::from_slice(v.value()).map_err(|e| format!("invalid routine state: {e}"))?,
            None => RoutineState::default(),
        };
        if now_ms < state.last_observed_ms { return Err("clock moved backwards".into()); }
        state.last_observed_ms = now_ms;
        let claim = if state.claim.is_some_and(|c| now_ms < c.expires_at_ms) || !should_run_daily_routine_with(policy, now_ms, state.last_success_ms, hour, minute) {
            None
        } else {
            state.generation = state.generation.checked_add(1).ok_or("claim generation exhausted")?;
            let claim = RoutineClaim { generation: state.generation, expires_at_ms };
            state.claim = Some(claim);
            Some(claim)
        };
        let bytes = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
        table.insert("state", bytes.as_slice()).map_err(|e| e.to_string())?;
        claim
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

/// Finish an unexpired matching lease; only success advances the cooldown.
/// Call only under `Store::with_db`, never from a nested Store callback.
pub fn finish_daily_routine(db: &redb::Database, claim: RoutineClaim, now_ms: u64, success: bool) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(ROUTINE_TABLE).map_err(|e| e.to_string())?;
        let mut state: RoutineState = {
            let value = table.get("state").map_err(|e| e.to_string())?.ok_or("routine state missing")?;
            serde_json::from_slice(value.value()).map_err(|e| format!("invalid routine state: {e}"))?
        };
        if now_ms < state.last_observed_ms || state.claim != Some(claim) || now_ms >= claim.expires_at_ms {
            return Err("stale, expired, or invalid routine claim".into());
        }
        state.last_observed_ms = now_ms;
        state.claim = None;
        if success { state.last_success_ms = now_ms; }
        let bytes = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
        table.insert("state", bytes.as_slice()).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// A conditional rollback proposal, not authorization to mutate the store.
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackPlan {
    pub revision_id: u64,
    pub expected_current: Note,
    pub restore: Note,
}

/// Plan only a single-note update whose exact after-image is still current.
/// Creation, status changes and grouped operations need graph-aware restoration.
pub fn plan_rollback(revision: &Revision, current: Option<&Note>) -> Result<RollbackPlan, String> {
    if revision.op != "update" { return Err("only single-note updates can be planned; grouped and lifecycle operations require store support".into()); }
    let before = revision.before.as_ref().ok_or("revision has no before image")?;
    let after = revision.after.as_ref().ok_or("revision has no after image")?;
    if before.id != revision.note_id || after.id != revision.note_id || before.kind != after.kind || before.status != NoteStatus::Active || after.status != NoteStatus::Active || before.supersedes != after.supersedes || before.created_at_ms != after.created_at_ms {
        return Err("inconsistent or lifecycle revision".into());
    }
    if current != Some(after) { return Err("revision is stale or current note is missing".into()); }
    Ok(RollbackPlan { revision_id: revision.rev_id, expected_current: after.clone(), restore: before.clone() })
}

/// Deliberately refuses mutation until Store exposes a locked atomic rollback.
pub fn rollback_revision(_store: &Store, _rev_id: u64) -> Result<String, String> {
    Err("rollback unavailable: store lacks an atomic, graph-validated, versioned rollback API".into())
}

/// Create mirror files for active notes in an existing caller-selected directory.
/// Existing files (including symlinks) are never opened or overwritten. A failure
/// may leave earlier newly-created files in place. Use a dedicated empty directory.
pub fn sync_obsidian_mirror(store: &Store, vault_dir: &Path) -> Result<usize, String> {
    let notes = store.load_all_notes()?;
    write_obsidian_mirror(&notes, vault_dir)
}

/// Validate and export the exact caller-supplied snapshot, without rereading the store.
/// Validation occurs before any output is created. Callers can additionally apply
/// their own policy to this same snapshot before passing it here.
pub fn write_obsidian_mirror(notes: &BTreeMap<u64, Note>, dir: &Path) -> Result<usize, String> {
    if !dir.is_dir() { return Err("mirror destination must be an existing directory".into()); }
    for note in notes.values().filter(|n| n.status == NoteStatus::Active) {
        memb_core::reject_secrets(&note.title).map_err(|e| e.to_string())?;
        memb_core::reject_secrets(&note.desc).map_err(|e| e.to_string())?;
        memb_core::validate_properties(&note.properties).map_err(|e| e.to_string())?;
    }
    let mut count = 0;
    for note in notes.values().filter(|n| n.status == NoteStatus::Active) {
        let kind = match note.kind { Kind::Topic => "topic", Kind::Knowledge => "knowledge", Kind::Claim => "claim" };
        let path = dir.join(format!("{kind}-{}.md", note.id));
        let properties = serde_json::to_string(&note.properties).map_err(|e| e.to_string())?;
        // JSON values are valid YAML scalars/collections and escape newlines.
        let title = serde_json::to_string(&note.title).map_err(|e| e.to_string())?;
        let mut text = format!("---\nid: {}\nkind: {kind}\ntitle: {title}\nproperties: {properties}\n---\n\n# {}\n\n{}\n", note.id, note.title, note.desc);
        if !note.parents.is_empty() {
            text.push_str("\n## Parents\n");
            for parent in &note.parents {
                if let Some(p) = notes.get(parent).filter(|p| p.status == NoteStatus::Active) {
                    let k = match p.kind { Kind::Topic => "topic", Kind::Knowledge => "knowledge", Kind::Claim => "claim" };
                    text.push_str(&format!("- [[{k}-{}]]\n", p.id));
                }
            }
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        file.write_all(text.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn note(id: u64) -> Note {
        Note { id, title: "Synthetic".into(), desc: "Test".into(), kind: Kind::Topic, status: NoteStatus::Active, parents: vec![], properties: BTreeMap::new(), supersedes: None, created_at_ms: 1, updated_at_ms: 1 }
    }
    #[test]
    fn configured_policy_moves_the_target_time_cooldown_and_catch_up() {
        let last = 1_000_000_000u64;
        let h = 3600 * 1000;
        let p = RoutinePolicy::new(9, 30, 12).unwrap();
        assert_eq!(RoutinePolicy::default(), RoutinePolicy::new(15, 45, 20).unwrap());
        // Before the target time and before the catch-up window: not due, even past the cooldown.
        assert!(!should_run_daily_routine_with(&p, last + 12 * h, last, 9, 29));
        assert!(should_run_daily_routine_with(&p, last + 12 * h, last, 9, 30));
        assert!(should_run_daily_routine_with(&p, last + 13 * h, last, 23, 59));
        // Cooldown still wins over the target time.
        assert!(!should_run_daily_routine_with(&p, last + 12 * h - 1, last, 12, 0));
        // 24-hour catch-up applies regardless of the clock.
        assert!(should_run_daily_routine_with(&p, last + 24 * h, last, 3, 0));
        // A cooldown longer than a day stretches the catch-up window to match it.
        let long = RoutinePolicy::new(0, 0, 48).unwrap();
        assert_eq!(long.overdue_ms, 48 * h);
        assert!(!should_run_daily_routine_with(&long, last + 47 * h, last, 0, 0));
        assert!(should_run_daily_routine_with(&long, last + 48 * h, last, 0, 0));
        for bad in [(24, 0, 20), (0, 60, 20), (0, 0, 0), (0, 0, 169)] {
            assert!(RoutinePolicy::new(bad.0, bad.1, bad.2).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn configured_policy_is_used_by_the_claim() {
        let dir = std::env::temp_dir().join(format!("memb-ops-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        let p = RoutinePolicy::new(9, 30, 12).unwrap();
        let t0 = 5_000_000_000u64;
        let h = 3600 * 1000;
        // First ever run is overdue by definition; record it as a success.
        let first = store.with_db(|db| claim_daily_routine_with(&p, db, t0, 9, 30, 1000)).unwrap().unwrap();
        store.with_db(|db| finish_daily_routine(db, first, t0 + 10, true)).unwrap();
        // 13h later: cooldown (12h) has passed, but 09:00 is before this policy's 09:30 target.
        let now = t0 + 13 * h;
        assert!(store.with_db(|db| claim_daily_routine_with(&p, db, now, 9, 0, 1000)).unwrap().is_none());
        assert!(store.with_db(|db| claim_daily_routine_with(&p, db, now + 1, 9, 30, 1000)).unwrap().is_some());
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scheduler_boundaries_and_bad_clocks() {
        let last = 100_000_000;
        assert!(!should_run_daily_routine(last + MIN_COOLDOWN_MS - 1, last, 23, 59));
        assert!(should_run_daily_routine(last + MIN_COOLDOWN_MS, last, 15, 45));
        assert!(!should_run_daily_routine(last + OVERDUE_MS - 1, last, 15, 44));
        assert!(should_run_daily_routine(last + OVERDUE_MS, last, 7, 0));
        assert!(!should_run_daily_routine(last - 1, last, 23, 0));
        assert!(!should_run_daily_routine(last + OVERDUE_MS, last, 24, 0));
        assert!(!should_run_daily_routine(last + 5 * 60_000, last, 15, 45));
    }
    #[test]
    fn routine_claims_persist_fence_expired_workers_and_record_only_success() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("memb-ops-claims-{}-{unique}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let now = 100_000_000;
        let first = store.with_db(|db| claim_daily_routine(db, now, 7, 0, 1000)).unwrap().unwrap();
        assert!(store.with_db(|db| claim_daily_routine(db, now + 1, 7, 0, 1000)).unwrap().is_none());
        assert!(store.with_db(|db| claim_daily_routine(db, now - 1, 7, 0, 1000)).is_err());
        let second = store.with_db(|db| claim_daily_routine(db, now + 1000, 7, 0, 1000)).unwrap().unwrap();
        assert_ne!(first.generation, second.generation);
        assert!(store.with_db(|db| finish_daily_routine(db, first, now + 1001, true)).is_err());
        store.with_db(|db| finish_daily_routine(db, second, now + 1001, false)).unwrap();
        let third = store.with_db(|db| claim_daily_routine(db, now + 1002, 7, 0, 1000)).unwrap().unwrap();
        store.with_db(|db| finish_daily_routine(db, third, now + 1003, true)).unwrap();
        assert!(store.with_db(|db| claim_daily_routine(db, now + 1004, 23, 0, 1000)).unwrap().is_none());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rollback_requires_exact_current_image_and_safe_operation() {
        let before = note(1);
        let mut after = before.clone(); after.desc = "changed".into();
        let mut rev = Revision { rev_id: 2, ts_ms: 2, op: "update".into(), note_id: 1, before: Some(before.clone()), after: Some(after.clone()) };
        assert_eq!(plan_rollback(&rev, Some(&after)).unwrap().restore, before);
        assert!(plan_rollback(&rev, Some(&before)).is_err());
        rev.op = "supersede_old".into();
        assert!(plan_rollback(&rev, Some(&after)).is_err());
    }
    #[test]
    fn mirror_rejects_sensitive_snapshot_before_writing() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("memb-ops-sensitive-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let mut sensitive = note(2);
        sensitive.properties.insert("password".into(), serde_json::json!("abcdefghijk"));
        let notes = BTreeMap::from([(1, note(1)), (2, sensitive)]);
        assert!(write_obsidian_mirror(&notes, &dir).is_err());
        assert!(!dir.join("topic-1.md").exists());
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn caller_snapshot_is_independent_of_later_changes() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("memb-ops-snapshot-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let mut current = BTreeMap::from([(1, note(1))]);
        let snapshot = current.clone();
        current.get_mut(&1).unwrap().desc = "password: abcdefghijk".into();
        write_obsidian_mirror(&snapshot, &dir).unwrap();
        let output = std::fs::read_to_string(dir.join("topic-1.md")).unwrap();
        assert!(output.contains("Test"));
        assert!(!output.contains("abcdefghijk"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn mirror_is_create_only() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("memb-ops-test-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let notes = BTreeMap::from([(1, note(1))]);
        let path = dir.join("topic-1.md");
        std::fs::write(&path, "user content").unwrap();
        assert!(write_obsidian_mirror(&notes, &dir).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user content");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(write_obsidian_mirror(&notes, &dir).unwrap(), 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("properties: {}"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
