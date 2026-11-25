use memb_core::{Note, NoteStatus};
use memb_store::{Revision, Store, NOTES_TABLE, REVISIONS_TABLE};
use redb::ReadableTable;
use std::fs;
use std::path::Path;

pub const MIN_COOLDOWN_MS: u64 = 20 * 3600 * 1000;
pub const OVERDUE_MS: u64 = 24 * 3600 * 1000;
pub const IDLE_SWEEP_AGE_MS: u64 = 24 * 3600 * 1000;
pub const IDLE_SWEEP_MIN_MESSAGES: usize = 45;

pub fn should_run_daily_routine(now_ms: u64, last_run_ms: u64, now_wib_hour: u8, now_wib_min: u8) -> bool {
    let elapsed = now_ms.saturating_sub(last_run_ms);
    if elapsed < MIN_COOLDOWN_MS {
        return false;
    }
    let is_overdue = elapsed >= OVERDUE_MS;
    let is_scheduled_slot = now_wib_hour > 15 || (now_wib_hour == 15 && now_wib_min >= 45);
    is_overdue || is_scheduled_slot
}

/// Restores a note to its `before` state recorded at `rev_id` in `revisions_v2`.
pub fn rollback_revision(store: &Store, rev_id: u64) -> Result<String, String> {
    let rev: Revision = {
        let r = store.db().begin_read().map_err(|e| e.to_string())?;
        let tbl = r.open_table(REVISIONS_TABLE).map_err(|e| e.to_string())?;
        let raw = tbl
            .get(rev_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("revision {rev_id} not found"))?;
        serde_json::from_slice(raw.value()).map_err(|e| e.to_string())?
    };

    let w = store.db().begin_write().map_err(|e| e.to_string())?;
    {
        let mut notes = w.open_table(NOTES_TABLE).map_err(|e| e.to_string())?;
        match rev.before {
            Some(prev) => {
                let bytes = serde_json::to_vec(&prev).map_err(|e| e.to_string())?;
                notes.insert(rev.note_id, bytes.as_slice()).map_err(|e| e.to_string())?;
            }
            None => {
                notes.remove(rev.note_id).map_err(|e| e.to_string())?;
            }
        }
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(format!("rolled back revision {rev_id} (note #{})", rev.note_id))
}

/// Writes an Obsidian-compatible Markdown mirror of all active notes with [[Title]] wikilinks.
pub fn sync_obsidian_mirror(store: &Store, vault_dir: &Path) -> Result<usize, String> {
    fs::create_dir_all(vault_dir).map_err(|e| e.to_string())?;
    let all = store.load_all_notes()?;
    let mut written = 0;
    for note in all.values().filter(|n| n.status == NoteStatus::Active) {
        let safe_name = sanitize_filename(&format!("{:05}-{}", note.id, note.title));
        let path = vault_dir.join(format!("{safe_name}.md"));
        let mut md = format!(
            "---\nid: {}\nkind: {:?}\nstatus: {:?}\n---\n\n# {}\n\n{}\n",
            note.id, note.kind, note.status, note.title, note.desc
        );
        if !note.parents.is_empty() {
            md.push_str("\n## Parents\n");
            for pid in &note.parents {
                if let Some(p) = all.get(pid) {
                    md.push_str(&format!("- [[{:05}-{}]]\n", p.id, sanitize_filename(&p.title)));
                }
            }
        }
        if !note.properties.is_empty() {
            md.push_str("\n## Properties\n```json\n");
            md.push_str(&serde_json::to_string_pretty(&note.properties).unwrap_or_default());
            md.push_str("\n```\n");
        }
        fs::write(&path, md).map_err(|e| e.to_string())?;
        written += 1;
    }
    Ok(written)
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_at_0700_runs_once_and_skips_1545_same_day() {
        let day1 = 1_700_000_000_000u64;
        let wake_0700 = day1 + 39 * 3600 * 1000;
        assert!(should_run_daily_routine(wake_0700, day1, 7, 0));
        let same_day_1545 = wake_0700 + (8 * 3600 + 45 * 60) * 1000;
        assert!(!should_run_daily_routine(same_day_1545, wake_0700, 15, 45));
    }

    #[test]
    fn wake_at_1540_runs_immediately_and_skips_1545() {
        let prev = 1_700_000_000_000u64;
        let wake_1540 = prev + 24 * 3600 * 1000;
        assert!(should_run_daily_routine(wake_1540, prev, 15, 40));
        assert!(!should_run_daily_routine(wake_1540 + 5 * 60 * 1000, wake_1540, 15, 45));
    }
}
