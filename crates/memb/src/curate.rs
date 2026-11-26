//! `memb curate` and `memb proposals`: the explicit, user-run curation commands.
//!
//! `memb curate` is the only place memb starts a model, and only the command the user
//! configured (`[curator] cli_command`). It sends staged conversation chunks to that
//! command, turns the answer into pending proposals and changes no note. A proposal
//! only changes a note when the user types its confirmation in `proposals apply`.
use crate::config::Config;
use memb_curator::backend::{self, Backend, PromptNote};
use memb_curator::{links, packets};
use memb_curator::ledger::{self, Status, StoredProposal};
use memb_curator::{CuratorAction, Proposal};
use memb_store::Store;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_CHUNK_TEXT_BYTES: usize = 12 * 1024;
const MAX_CHUNKS_PER_RUN: usize = 100;
/// Most chunks summarized (one model call each) in one run.
const MAX_SUMMARIES_PER_RUN: usize = 20;

/// The model command for `cfg`, started in an empty sandbox directory under the data
/// directory so the model session has no project files to read or change.
pub fn backend_for(cfg: &Config, data_root: &std::path::Path) -> Result<Backend, String> {
    let sandbox = data_root.join("curator-sandbox");
    std::fs::create_dir_all(&sandbox).map_err(|_| "could not create the curator sandbox directory")?;
    let backend = Backend::from_command(&cfg.curator.cli_command)?.with_cwd(sandbox);
    if cfg.curator.cli_protocol == "agy-stream" {
        let home = data_root.join("curator-home");
        std::fs::create_dir_all(&home).map_err(|_| "could not create the curator home directory")?;
        return Ok(backend.with_agy_stream(home));
    }
    Ok(backend)
}

pub fn ttl_ms(cfg: &Config) -> u64 {
    u64::from(cfg.curator.staging_ttl_days) * DAY_MS
}

/// Enforces the staging limit on chunks and pending proposals. Returns how many of
/// each expired. Raw chunk text is never deleted.
pub fn expire_old(store: &Store, cfg: &Config, now: u64) -> Result<(usize, usize), String> {
    let ttl = ttl_ms(cfg);
    let chunks = store.with_db(|db| memb_scroll::expire_staged_chunks(db, now, ttl))?;
    let proposals = store.with_db(|db| ledger::expire(db, now, ttl))?;
    Ok((chunks, proposals))
}

fn flatten(messages: &[memb_scroll::ScrollMessage]) -> String {
    let mut text = String::new();
    for m in messages {
        let role = match m.role {
            memb_scroll::Role::User => "user",
            memb_scroll::Role::Assistant => "assistant",
        };
        text.push_str(role);
        text.push_str(": ");
        text.push_str(&m.content);
        text.push('\n');
        if text.len() > MAX_CHUNK_TEXT_BYTES {
            let mut end = MAX_CHUNK_TEXT_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n[chunk text cut]\n");
            break;
        }
    }
    text
}

#[derive(Debug)]
pub struct Summary {
    pub staged_now: usize,
    pub expired_chunks: usize,
    pub expired_proposals: usize,
    /// Chunks in the evidence packets sent (or that a dry run would send).
    pub chunks_sent: usize,
    pub prompt_bytes: usize,
    /// Evidence packets sent (or that a dry run would send).
    pub packets: usize,
    /// Packets left out because the model already judged identical content.
    pub packets_already_judged: usize,
    /// Links stored this run: (chunk to chunk, chunk to note).
    pub links: (usize, usize),
    /// Chunks that got a summary this run (a dry run reports how many would).
    pub summarized: usize,
    /// Chunks that got a summary vector this run.
    pub embedded: usize,
    pub proposals: Vec<u64>,
    /// Proposals memb applied by itself this run (`curator.auto_apply`).
    pub auto_applied: Vec<u64>,
    /// Proposals left pending for review: over the per-run limit, a cascade, or paused.
    pub held_for_review: usize,
    pub skipped: usize,
    pub dry_run: bool,
}

impl Summary {
    pub fn to_json(&self) -> Value {
        json!({"dry_run": self.dry_run, "staged_now": self.staged_now,
               "expired_chunks": self.expired_chunks, "expired_proposals": self.expired_proposals,
               "chunks_sent": self.chunks_sent, "prompt_bytes": self.prompt_bytes,
               "packets": self.packets, "packets_already_judged": self.packets_already_judged,
               "chunk_links": self.links.0, "note_links": self.links.1,
               "summarized": self.summarized, "embedded": self.embedded,
               "proposals": self.proposals, "auto_applied": self.auto_applied,
               "held_for_review": self.held_for_review, "skipped": self.skipped})
    }
}

/// One curation run. With `dry_run` nothing is sent and nothing but staging and
/// expiry is recorded, so the user can see how much text would leave memb.
///
/// Order: expire, stage, Layer 1 (summaries and summary vectors), links between staged
/// chunks and between chunks and notes, evidence packets, then one model call for the
/// packets it has not already judged. Chunks that meet none of the three Phase 3 rules
/// are in no packet and stay staged until the staging limit.
pub fn run_curate(
    store: &Store,
    cfg: &Config,
    backend: &Backend,
    now: u64,
    dry_run: bool,
    notice: &mut impl Write,
) -> Result<Summary, String> {
    if std::env::var("MEMB_INTERNAL").ok().as_deref() == Some("1") {
        // Recursion guard: a curation command must not start another curation.
        return Err("refusing to curate from inside a curation session".into());
    }
    let (expired_chunks, expired_proposals) = expire_old(store, cfg, now)?;
    let mut staged_now = 0;
    for chunk in store.with_db(|db| memb_scroll::list_uncurated_chunks(db, MAX_CHUNKS_PER_RUN))? {
        if store.with_db(|db| memb_scroll::stage_chunk_evidence(db, &chunk.chunk_id))? {
            staged_now += 1;
        }
    }
    let mut staged = store.with_db(|db| memb_scroll::list_staged_chunks(db, MAX_CHUNKS_PER_RUN))?;
    let (mut summarized, mut embedded) = (0, 0);
    if dry_run {
        summarized = staged.iter().filter(|c| c.summary.is_none()).count().min(MAX_SUMMARIES_PER_RUN);
    } else if !staged.is_empty() {
        // A run can add up to MAX_PROPOSALS_PER_RUN proposals: refuse before sending anything
        // if the pending list could not hold them, so no text is spent on proposals that
        // cannot be saved.
        let pending = store.with_db(|db| ledger::list(db, Some(&Status::Pending), 200))?.len();
        if pending + backend::MAX_PROPOSALS_PER_RUN > ledger::MAX_PENDING {
            return Err("too many pending proposals; apply or reject some first".into());
        }
        (summarized, embedded) = summarize_staged(store, backend, &staged, notice)?;
        staged = store.with_db(|db| memb_scroll::list_staged_chunks(db, MAX_CHUNKS_PER_RUN))?;
    }
    let all_notes = store.load_all_notes()?;
    // Layer 2 links: no model call. A dry run only reads the links stored earlier.
    let mut links_made = (0, 0);
    if !dry_run && !staged.is_empty() {
        // Best effort: with no embedding backend there are no vectors to link.
        if let Ok(Some((tag, nodes))) = memb_rag::note_vectors(store) {
            links_made = store.with_db(|db| links::refresh_links(db, &staged, &nodes, &tag))?;
        }
    }
    let ids: Vec<String> = staged.iter().map(|c| c.chunk_id.clone()).collect();
    let link_set = store.with_db(|db| links::read_links(db, &ids))?;
    let all_packets = packets::build_linked_packets(&staged, &link_set, &all_notes);
    let ttl = ttl_ms(cfg);
    let mut fresh = Vec::new();
    let mut already_judged = 0;
    for packet in all_packets {
        if store.with_db(|db| links::hash_seen(db, &packet.hash, now, ttl))? {
            already_judged += 1;
        } else {
            fresh.push(packet);
        }
    }
    let catalog: Vec<PromptNote> = all_notes
        .values()
        .filter(|n| n.status == memb_core::NoteStatus::Active)
        .map(|n| PromptNote {
            id: n.id,
            kind: format!("{:?}", n.kind).to_lowercase(),
            title: n.title.clone(),
        })
        .collect();
    let (prompt, included) = backend::build_packet_prompt(&fresh, &catalog);
    let sent: Vec<&packets::LinkedPacket> = included.iter().map(|&i| &fresh[i]).collect();
    let mut summary = Summary {
        staged_now,
        expired_chunks,
        expired_proposals,
        chunks_sent: sent.iter().map(|p| p.chunks.len()).sum(),
        prompt_bytes: prompt.len(),
        packets: sent.len(),
        packets_already_judged: already_judged,
        links: links_made,
        summarized,
        embedded,
        proposals: Vec::new(),
        auto_applied: Vec::new(),
        held_for_review: 0,
        skipped: 0,
        dry_run,
    };
    if !dry_run {
        // Recorded whether or not anything is sent, so the "enough chunks" trigger does not
        // fire again every minute for chunks the model already judged, and a failing model
        // is not retried at once.
        store.with_db(|db| links::set_last_curation_ms(db, now))?;
    }
    if sent.is_empty() || dry_run {
        return Ok(summary);
    }
    // Defense in depth: capture already refuses credentials; never send one onward.
    memb_core::reject_secrets(&prompt).map_err(|_| "sensitive text found; nothing was sent")?;
    writeln!(
        notice,
        "memb: sending {} evidence packet(s) with {} chunk(s) ({} bytes) of private conversation text to `{}`",
        sent.len(),
        summary.chunks_sent,
        prompt.len(),
        backend.program()
    )
    .map_err(|_| "output failed")?;
    let reply = backend.run(&prompt)?;
    let allowed: BTreeMap<String, String> = sent
        .iter()
        .flat_map(|p| p.chunks.iter().map(|c| (c.chunk_id.clone(), c.session_id.clone())))
        .collect();
    let (proposals, mut skipped) = backend::parse_response(&reply, &allowed)?;
    let mut cited: Vec<String> = Vec::new();
    for p in proposals {
        // The proposal is validated now and again when applied.
        if memb_core::reject_secrets(&p.action_json.to_string()).is_err()
            || Proposal::prepare(p.action.clone(), &p.sources).is_err()
        {
            skipped += 1;
            continue;
        }
        let id = store.with_db(|db| ledger::add(db, now, p.action_json.clone(), p.sources.clone()))?;
        summary.proposals.push(id);
        cited.extend(p.sources.iter().map(|(_, chunk)| chunk.clone()));
    }
    summary.skipped = skipped;
    // The model judged these packets: an identical packet is not sent again until it
    // changes or the entry expires. Only chunks that a saved proposal cites leave staging;
    // the rest were deferred and stay staged until the staging limit.
    let hashes: Vec<String> = sent.iter().map(|p| p.hash.clone()).collect();
    store.with_db(|db| links::record_hashes(db, &hashes, now, ttl))?;
    cited.sort();
    cited.dedup();
    store.with_db(|db| memb_scroll::mark_chunks_curated(db, &cited))?;
    if cfg.curator.auto_apply && !summary.proposals.is_empty() {
        let (applied, held) = auto_apply(store, cfg, &summary.proposals, now, notice)?;
        summary.auto_applied = applied;
        summary.held_for_review = held;
    }
    Ok(summary)
}

/// The store revisions an action writes, so they can be recorded and reversed together.
fn revision_ops(action: &CuratorAction) -> &'static [&'static str] {
    match action {
        CuratorAction::Defer {} => &[],
        CuratorAction::KnowledgeUpsert { .. } | CuratorAction::ClaimAdd { .. } => &["remember"],
        CuratorAction::ClaimSupersede { .. } => &["supersede_old", "supersede_new"],
        CuratorAction::Update { .. } => &["update"],
        CuratorAction::Retract { .. } => &["retract"],
    }
}

/// Claims `id`, applies it and records the revisions it wrote. The caller has the user's
/// typed confirmation or, with `auto`, the `curator.auto_apply` setting.
fn apply_recorded(store: &Store, id: u64, prepared: Proposal, now: u64, auto: bool) -> Result<Option<u64>, String> {
    // Claim first: another process (or a second confirmation) cannot apply it again.
    store.with_db(|db| ledger::claim(db, id, now))?;
    let ops = revision_ops(prepared.action());
    let mark = store.latest_revision_id()?;
    match prepared.apply_after_user_approval(store) {
        Ok(note) => {
            let revisions: Vec<u64> = store
                .list_revisions(None, 200)?
                .into_iter()
                .filter(|r| r.rev_id > mark && ops.contains(&r.op.as_str()))
                .map(|r| r.rev_id)
                .rev()
                .collect();
            store.with_db(|db| ledger::finish_recorded(db, id, now, note, revisions, auto))?;
            Ok(note)
        }
        Err(reason) => {
            // It cannot be applied as proposed (for example the note changed); it is
            // not retried silently.
            let _ = store.with_db(|db| ledger::reject(db, id, now));
            Err(reason)
        }
    }
}

/// True when the action would change more than one note or remove a whole subtree; those
/// are never applied automatically.
fn is_cascade(store: &Store, action: &CuratorAction) -> Result<bool, String> {
    let CuratorAction::Retract { id, .. } = action else { return Ok(false) };
    let all = store.load_all_notes()?;
    let Some(target) = all.get(id) else { return Ok(false) };
    Ok(match target.kind {
        memb_core::Kind::Topic => true,
        memb_core::Kind::Knowledge => all
            .values()
            .any(|n| n.status == memb_core::NoteStatus::Active && n.parents.contains(id)),
        memb_core::Kind::Claim => false,
    })
}

/// Applies the proposals of one run by itself, within the rails: not while paused, at most
/// `curator.max_auto_edits` per run, never a cascading retraction, and a secret check on
/// the exact action. Anything held stays pending for `proposals apply`. Returns the ids
/// applied and the number held.
fn auto_apply(
    store: &Store,
    cfg: &Config,
    ids: &[u64],
    now: u64,
    notice: &mut impl Write,
) -> Result<(Vec<u64>, usize), String> {
    if store.with_db(links::auto_apply_paused)? {
        writeln!(notice, "memb: auto-apply is paused; {} proposal(s) left pending", ids.len())
            .map_err(|_| "output failed")?;
        return Ok((Vec::new(), ids.len()));
    }
    let limit = cfg.curator.max_auto_edits as usize;
    let (mut applied, mut held) = (Vec::new(), 0);
    for &id in ids {
        let Some(p) = store.with_db(|db| ledger::get(db, id))? else { continue };
        let ready = (|| -> Result<Option<Proposal>, String> {
            let action: CuratorAction = serde_json::from_value(p.action.clone()).map_err(|e| e.to_string())?;
            if applied.len() >= limit || is_cascade(store, &action)? {
                return Ok(None);
            }
            memb_core::reject_secrets(&p.action.to_string()).map_err(|_| "sensitive text".to_string())?;
            Ok(Some(Proposal::prepare(action, &p.sources)?))
        })();
        match ready {
            Ok(Some(prepared)) => match apply_recorded(store, id, prepared, now, true) {
                Ok(_) => applied.push(id),
                Err(reason) => {
                    writeln!(notice, "memb: proposal {id} could not be applied and was rejected: {reason}")
                        .map_err(|_| "output failed")?;
                }
            },
            _ => held += 1,
        }
    }
    writeln!(
        notice,
        "memb: auto-applied {} proposal(s), {held} left for review (undo with `memb proposals rollback <id>`)",
        applied.len()
    )
    .map_err(|_| "output failed")?;
    Ok((applied, held))
}

/// Reverses every note edit a proposal made, after a typed `ROLLBACK <id>`. Returns false
/// when the user did not confirm.
pub fn rollback_interactive(
    store: &Store,
    id: u64,
    now: u64,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool, String> {
    let p = store.with_db(|db| ledger::get(db, id))?.ok_or("proposal not found")?;
    if p.status != Status::Applied {
        return Err(format!("proposal is {:?}, not applied", p.status).to_lowercase());
    }
    if p.revisions.is_empty() {
        return Err("this proposal recorded no note edits (applied before rollback records existed)".into());
    }
    let revisions: Vec<Value> = p
        .revisions
        .iter()
        .filter_map(|r| store.revision(*r).ok().flatten())
        .map(|r| {
            json!({"revision": r.rev_id, "op": r.op, "note": r.note_id,
                   "before": r.before.map(|n| json!({"title": n.title, "status": n.status})),
                   "after": r.after.map(|n| json!({"title": n.title, "status": n.status}))})
        })
        .collect();
    let display = serde_json::to_string(&json!({"proposal": brief(&p), "undoes": revisions}))
        .map_err(|_| "preview failed")?;
    writeln!(output, "{display}\nType ROLLBACK {id} to confirm, or anything else to cancel:")
        .map_err(|_| "output failed")?;
    output.flush().map_err(|_| "output failed")?;
    let mut line = String::new();
    input.read_line(&mut line).map_err(|_| "input failed")?;
    if line.trim_end_matches(['\r', '\n']) != format!("ROLLBACK {id}") {
        return Ok(false);
    }
    store.rollback_revisions(&p.revisions)?;
    store.with_db(|db| ledger::mark_rolled_back(db, id, now))?;
    writeln!(output, "Rolled back proposal {id}.").map_err(|_| "output failed")?;
    Ok(true)
}

/// Uncurated chunks (staged or not) that make a curation run due between the daily
/// routines.
pub const THRESHOLD_CHUNKS: usize = 3;
/// Least time between two curation runs started by that trigger.
pub const THRESHOLD_COOLDOWN_MS: u64 = 60 * 60 * 1000;

/// Phase 3: curation also runs "whenever >= 3 uncurated chunks accumulate", not only
/// after the daily sweep. True when there are at least [`THRESHOLD_CHUNKS`] such chunks
/// and the last run (of any kind) was at least an hour ago.
pub fn threshold_due(store: &Store, now: u64) -> Result<bool, String> {
    let unstaged = store.with_db(|db| memb_scroll::list_uncurated_chunks(db, THRESHOLD_CHUNKS))?.len();
    let staged = store.with_db(|db| memb_scroll::list_staged_chunks(db, THRESHOLD_CHUNKS))?.len();
    if unstaged + staged < THRESHOLD_CHUNKS {
        return Ok(false);
    }
    let last = store.with_db(links::last_curation_ms)?;
    Ok(now.saturating_sub(last) >= THRESHOLD_COOLDOWN_MS)
}

/// Layer 1: one model call per staged chunk that has no summary yet (at most
/// `MAX_SUMMARIES_PER_RUN`), then a local embedding of each summary. A model that
/// cannot start or fails aborts the run before anything is recorded for that chunk; a
/// reply that cannot be used skips only that chunk, which is retried next run. The
/// embedding is best-effort: with no embedding backend the chunk simply has no vector.
/// Returns (summarized, embedded) counts.
fn summarize_staged(
    store: &Store,
    backend: &Backend,
    staged: &[memb_scroll::ScrollChunk],
    notice: &mut impl Write,
) -> Result<(usize, usize), String> {
    let todo: Vec<&memb_scroll::ScrollChunk> = staged
        .iter()
        .filter(|c| c.summary.is_none())
        .take(MAX_SUMMARIES_PER_RUN)
        .collect();
    let (mut summarized, mut embedded) = (0, 0);
    if !todo.is_empty() {
        writeln!(
            notice,
            "memb: sending {} chunk(s) of private conversation text to `{}` for summaries",
            todo.len(),
            backend.program()
        )
        .map_err(|_| "output failed")?;
    }
    for chunk in todo {
        let prompt = backend::build_summary_prompt(&flatten(&chunk.messages));
        memb_core::reject_secrets(&prompt).map_err(|_| "sensitive text found; nothing was sent")?;
        let reply = backend.run(&prompt)?;
        let Ok((text, explicit)) = backend::parse_summary(&reply) else { continue };
        if memb_core::reject_secrets(&text).is_err() {
            continue;
        }
        if store.with_db(|db| memb_scroll::set_chunk_summary(db, &chunk.chunk_id, &text, explicit))? {
            summarized += 1;
        }
    }
    // Vectors for every staged chunk that has a summary but no vector yet.
    let fresh = store.with_db(|db| memb_scroll::list_staged_chunks(db, MAX_CHUNKS_PER_RUN))?;
    for chunk in fresh.iter().filter(|c| c.summary_vec.is_none()) {
        let Some(text) = &chunk.summary else { continue };
        let Ok(Some((model, vector))) = memb_rag::embed_text(text, false) else { continue };
        let stored = memb_scroll::ChunkVec { model, scale: vector.scale, data: vector.data };
        if store.with_db(|db| memb_scroll::set_chunk_vector(db, &chunk.chunk_id, stored))? {
            embedded += 1;
        }
    }
    Ok((summarized, embedded))
}

fn action_name(action: &Value) -> &str {
    action["action"].as_str().unwrap_or("?")
}

fn brief(p: &StoredProposal) -> Value {
    let target = p.action.get("id").or_else(|| p.action.get("old_id")).cloned().unwrap_or(Value::Null);
    json!({"id": p.id, "status": p.status, "created_ms": p.created_ms,
           "action": action_name(&p.action), "note": target, "sources": p.sources.len(),
           "applied_note": p.note_id})
}

pub fn list(store: &Store, status: Option<Status>, limit: usize) -> Result<Vec<Value>, String> {
    let rows = store.with_db(|db| ledger::list(db, status.as_ref(), limit))?;
    Ok(rows.iter().map(brief).collect())
}

/// Everything the user needs to judge one proposal: the exact action, its sources and
/// the current text of the note it would change.
pub fn show(store: &Store, id: u64) -> Result<Value, String> {
    let p = store
        .with_db(|db| ledger::get(db, id))?
        .ok_or("proposal not found")?;
    let target = p
        .action
        .get("id")
        .or_else(|| p.action.get("old_id"))
        .and_then(Value::as_u64);
    let current = match target {
        Some(note) => store.inspect(note)?,
        None => None,
    };
    Ok(json!({"proposal": brief(&p), "action": p.action,
              "sources": p.sources.iter().map(|(s, c)| json!({"session_id": s, "chunk_id": c})).collect::<Vec<_>>(),
              "current_note": current,
              "notice": "Review the exact action. Applying needs the typed confirmation and can be done once."}))
}

/// Proposals memb applied by itself (newest first) with the revisions each wrote.
pub fn audit(store: &Store, limit: usize) -> Result<Vec<Value>, String> {
    let rows = store.with_db(|db| ledger::list(db, None, 200))?;
    Ok(rows
        .iter()
        .filter(|p| p.auto && matches!(p.status, Status::Applied | Status::RolledBack))
        .take(limit)
        .map(|p| {
            let mut row = brief(p);
            row["revisions"] = json!(p.revisions);
            row
        })
        .collect())
}

pub fn reject(store: &Store, id: u64, now: u64) -> Result<(), String> {
    store.with_db(|db| ledger::reject(db, id, now))
}

/// Shows the proposal, asks for `APPLY <id>`, then applies it exactly once. Returns
/// false when the user did not confirm.
pub fn apply_interactive(
    store: &Store,
    cfg: &Config,
    id: u64,
    now: u64,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool, String> {
    expire_old(store, cfg, now)?;
    let proposal = store
        .with_db(|db| ledger::get(db, id))?
        .ok_or("proposal not found")?;
    if proposal.status != Status::Pending {
        return Err(format!("proposal is {:?}, not pending", proposal.status).to_lowercase());
    }
    let action: CuratorAction = serde_json::from_value(proposal.action.clone())
        .map_err(|_| "stored proposal is not a valid action")?;
    let prepared = Proposal::prepare(action, &proposal.sources)?;
    let display = serde_json::to_string(&show(store, id)?).map_err(|_| "preview failed")?;
    memb_core::reject_secrets(&display).map_err(|_| "sensitive preview rejected")?;
    writeln!(output, "{display}\nType APPLY {id} to confirm, or anything else to cancel:")
        .map_err(|_| "output failed")?;
    output.flush().map_err(|_| "output failed")?;
    let mut line = String::new();
    input.read_line(&mut line).map_err(|_| "input failed")?;
    if line.trim_end_matches(['\r', '\n']) != format!("APPLY {id}") {
        return Ok(false);
    }
    match apply_recorded(store, id, prepared, now, false) {
        Ok(note) => {
            writeln!(output, "Applied; note {}.", note.map_or_else(|| "unchanged".into(), |n| n.to_string()))
                .map_err(|_| "output failed after apply")?;
            Ok(true)
        }
        Err(reason) => Err(format!("could not be applied and was rejected: {reason}")),
    }
}

fn parse_status(name: &str) -> Option<Status> {
    match name {
        "pending" => Some(Status::Pending),
        "applying" => Some(Status::Applying),
        "applied" => Some(Status::Applied),
        "rolledback" | "rolled-back" => Some(Status::RolledBack),
        "rejected" => Some(Status::Rejected),
        "expired" => Some(Status::Expired),
        _ => None,
    }
}

pub const CURATE_USAGE: &str = "Usage: memb curate [--dry-run]";
pub const PROPOSALS_USAGE: &str = "Usage: memb proposals [--status pending|applying|applied|rejected|expired] [--limit <1-200>] | memb proposals show <id> | memb proposals apply <id> | memb proposals reject <id> | memb proposals rollback <id> | memb proposals audit [<limit>] | memb proposals pause | memb proposals resume";

/// What a `proposals` command line asks for.
#[derive(Debug, PartialEq)]
pub enum ProposalsCommand {
    List(Option<Status>, usize),
    Show(u64),
    Apply(u64),
    Reject(u64),
    /// Undo every note edit of an applied proposal (typed confirmation).
    Rollback(u64),
    /// What memb applied by itself, newest first.
    Audit(usize),
    /// Kill switch for auto-apply, and its release.
    Pause,
    Resume,
}

pub fn parse_proposals(args: &[String]) -> Option<ProposalsCommand> {
    let id = |s: &String| s.parse::<u64>().ok().filter(|n| *n > 0);
    let limit = |s: &String| s.parse::<usize>().ok().filter(|n| (1..=200).contains(n));
    match args {
        [] => Some(ProposalsCommand::List(None, 20)),
        [c, i] if c == "show" => id(i).map(ProposalsCommand::Show),
        [c, i] if c == "apply" => id(i).map(ProposalsCommand::Apply),
        [c, i] if c == "reject" => id(i).map(ProposalsCommand::Reject),
        [c, i] if c == "rollback" => id(i).map(ProposalsCommand::Rollback),
        [c] if c == "audit" => Some(ProposalsCommand::Audit(20)),
        [c, n] if c == "audit" => limit(n).map(ProposalsCommand::Audit),
        [c] if c == "pause" => Some(ProposalsCommand::Pause),
        [c] if c == "resume" => Some(ProposalsCommand::Resume),
        [f, s] if f == "--status" => parse_status(s).map(|s| ProposalsCommand::List(Some(s), 20)),
        [f, n] if f == "--limit" => limit(n).map(|n| ProposalsCommand::List(None, n)),
        [f1, s, f2, n] if f1 == "--status" && f2 == "--limit" => {
            parse_status(s).zip(limit(n)).map(|(s, n)| ProposalsCommand::List(Some(s), n))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn proposals_command_lines_are_parsed_strictly() {
        assert_eq!(parse_proposals(&[]), Some(ProposalsCommand::List(None, 20)));
        assert_eq!(parse_proposals(&strings(&["show", "3"])), Some(ProposalsCommand::Show(3)));
        assert_eq!(parse_proposals(&strings(&["apply", "12"])), Some(ProposalsCommand::Apply(12)));
        assert_eq!(parse_proposals(&strings(&["reject", "1"])), Some(ProposalsCommand::Reject(1)));
        assert_eq!(
            parse_proposals(&strings(&["--status", "pending", "--limit", "5"])),
            Some(ProposalsCommand::List(Some(Status::Pending), 5))
        );
        for bad in [
            &["apply", "0"][..],
            &["apply", "x"],
            &["apply"],
            &["--limit", "201"],
            &["--limit", "0"],
            &["--status", "open"],
            &["delete", "1"],
            &["show", "1", "2"],
        ] {
            assert_eq!(parse_proposals(&strings(bad)), None, "{bad:?}");
        }
    }

    fn temp_store(name: &str) -> (Store, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("memb-curate-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (Store::open(&root).unwrap(), root)
    }

    #[test]
    fn staging_limit_expires_chunks_and_pending_proposals_and_blocks_applying() {
        let (store, root) = temp_store("ttl");
        let mut cfg = Config::default();
        cfg.curator.staging_ttl_days = 30;
        let limits = memb_scroll::ScrollLimits { max_messages: 10, retain_tail: 2, ..Default::default() };
        for i in 0..10 {
            store.with_db(|db| memb_scroll::append_message_with(db, "s1", "cc", memb_scroll::Role::User, format!("message {i}"), true, &limits)).unwrap();
        }
        let chunk = store.with_db(|db| memb_scroll::list_uncurated_chunks(db, 5)).unwrap().remove(0);
        assert!(store.with_db(|db| memb_scroll::stage_chunk_evidence(db, &chunk.chunk_id)).unwrap());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let id = store
            .with_db(|db| ledger::add(db, now, json!({"action":"KNOWLEDGE_UPSERT","title":"t","desc":"d","parents":[1]}), vec![("s1".into(), chunk.chunk_id.clone())]))
            .unwrap();

        assert_eq!(expire_old(&store, &cfg, now + 29 * DAY_MS).unwrap(), (0, 0));
        // Past the limit: the staged chunk and the unanswered proposal both expire, and
        // applying is refused even with a correct confirmation.
        let mut input = std::io::Cursor::new(format!("APPLY {id}
"));
        let mut output = Vec::new();
        let err = apply_interactive(&store, &cfg, id, now + 31 * DAY_MS, &mut input, &mut output).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        assert!(output.is_empty(), "nothing was shown or asked for an expired proposal");
        assert!(store.with_db(|db| memb_scroll::list_staged_chunks(db, 5)).unwrap().is_empty());
        assert_eq!(store.load_all_notes().unwrap().len(), 0);
        // The raw chunk is kept for the owner; it is only no longer offered for curation.
        assert!(store.with_db(|db| memb_scroll::read_chunk(db, &chunk.chunk_id, 0, 1)).unwrap().is_some());
        // A configured shorter limit is honored.
        cfg.curator.staging_ttl_days = 1;
        assert_eq!(ttl_ms(&cfg), DAY_MS);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn three_uncurated_chunks_make_curation_due_at_most_once_an_hour() {
        let (store, root) = temp_store("threshold");
        let limits = memb_scroll::ScrollLimits { max_messages: 10, retain_tail: 2, ..Default::default() };
        let add_chunk = |session: &str| {
            for i in 0..10 {
                store.with_db(|db| memb_scroll::append_message_with(db, session, "cc", memb_scroll::Role::User, format!("m{i}"), true, &limits)).unwrap();
            }
        };
        let now = 10 * DAY_MS;
        add_chunk("a");
        add_chunk("b");
        assert!(!threshold_due(&store, now).unwrap(), "two chunks are not enough");
        add_chunk("c");
        assert!(threshold_due(&store, now).unwrap(), "three uncurated chunks, never curated");
        // Staged chunks still count (they are uncurated).
        for c in store.with_db(|db| memb_scroll::list_uncurated_chunks(db, 10)).unwrap() {
            store.with_db(|db| memb_scroll::stage_chunk_evidence(db, &c.chunk_id)).unwrap();
        }
        assert!(threshold_due(&store, now).unwrap());
        // A run just happened: not due again until an hour has passed.
        store.with_db(|db| links::set_last_curation_ms(db, now)).unwrap();
        assert!(!threshold_due(&store, now + THRESHOLD_COOLDOWN_MS - 1).unwrap());
        assert!(threshold_due(&store, now + THRESHOLD_COOLDOWN_MS).unwrap());
        // Curated chunks no longer count.
        let ids: Vec<String> = store.with_db(|db| memb_scroll::list_staged_chunks(db, 10)).unwrap().into_iter().map(|c| c.chunk_id).collect();
        store.with_db(|db| memb_scroll::mark_chunks_curated(db, &ids[..2])).unwrap();
        assert!(!threshold_due(&store, now + 2 * THRESHOLD_COOLDOWN_MS).unwrap());
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn chunk_text_is_cut_on_a_character_boundary() {
        let messages = vec![memb_scroll::ScrollMessage {
            role: memb_scroll::Role::User,
            content: "é".repeat(MAX_CHUNK_TEXT_BYTES),
            ts_ms: 0,
        }];
        let text = flatten(&messages);
        assert!(text.len() < MAX_CHUNK_TEXT_BYTES + 40);
        assert!(text.ends_with("[chunk text cut]\n"));
    }
}
