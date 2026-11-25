# Operations: daemon, mirror, history and rollback

These commands run on your machine against the local database. None of them contacts a network service on its own and none installs itself. The daemon, the mirror and `revisions` never change a note; the only command here that does is `rollback`, and only after you type its confirmation.

## `memb daemon`

```powershell
.\memb.exe daemon [--mirror-out <existing-directory>]
```

A foreground loop for a background process or a scheduled task. **memb does not install the scheduled task** (see "Not done yet" below); you start the process yourself.

- **Start:** applies Windows' 2 GiB job-object memory cap to itself, reads [config.toml](Configuration.md), opens the store, waits 30 seconds, then checks once a minute.
- **When it runs:** once a day, at or after the configured WIB time (default 15:45, UTC+7), at least `cooldown_hours` (default 20) after the last *successful* run, and in any case once 24 hours (or the cooldown, if longer) have passed since then. The very first run is due immediately.
- **One run at a time:** a run takes a 30-minute lease stored in the database, so a second daemon (or a restart) does not start the routine while a lease is held. The lease is not renewed: a run that takes longer than 30 minutes can overlap a later claim, but it is then refused when it tries to record its result, so it can never mark the routine successful. A clock that moves backwards is refused.
- **A run does, in order:**
  1. **Sweep:** scrolls idle longer than `idle_sweep_hours` (default 24) with at least `idle_sweep_min_messages` (default 45) messages become chunks; the last `scroll.retain_tail` messages stay live. At most 100 scrolls per run.
  2. **Stage:** up to 100 uncurated chunks are recorded as staged evidence. This only marks them; it does not summarize, embed or write notes.
  3. **Mirror** (only if a directory is configured): writes a new `mirror-<timestamp>` folder under it (see below).
- **Success is recorded only if every step succeeded.** After a failure the daemon waits 15 minutes before trying again, because a failed run does not start the cooldown.
- **Messages** go to stderr as one-line status notes (`routine done; swept N, staged M`); they never contain note text.

### Not done yet

- **No curation step.** The manual's "Layer 2 curation" (model-written proposals from staged chunks) is not part of the daemon; curator tools exist for a person to preview and approve one change at a time. This is tracked as a separate ticket.
- **No scheduled task.** The kit's `install-windows-task.ps1` has not been adopted: it needs review (for example, Windows' default three-day execution limit on a task would stop a long-running daemon) and your explicit approval before anything is registered.
- **No consolidation or compaction.** Retraction and deletion are logical; the database file is not shrunk.

## Obsidian mirror

```powershell
.\memb.exe mirror --out <existing-empty-directory>
```

Writes one Markdown file per active note (retracted and superseded notes get no file) into an **existing, empty directory outside the data directory** (create it first), with a link to each of the note's active parents. It is create-only: it never overwrites, updates or deletes a file and is not a sync; edits made in Obsidian are never imported. The snapshot is checked for credentials first and refused if any are found. A failure part-way can leave partial output, so use a fresh directory each time. The daemon does the same into `mirror-<timestamp>` subfolders.

## Revision history and rollback

Every `remember`, `update`, `supersede` (two revisions: the old and the new note) and `retract` records numbered revisions with before and after images.

```powershell
.\memb.exe revisions                 # newest first, up to 20
.\memb.exe revisions --note 12 --limit 50
.\memb.exe rollback --rev 31
```

- `revisions` prints one JSON line per revision: `revision`, `ts_ms`, `op`, `note`, the fields that `changed` (`title`, `desc`, `parents`, `properties`, `status`) and a `rollback` hint that depends only on the operation type (`update` revisions are eligible for a preview, everything else says "not supported"); the preview can still refuse an eligible one, for example if the note has changed since. **It never prints note text.** The limit is 1 to 200.
- `rollback --rev <id>` shows the exact current note and the note it would restore, then asks you to type `ROLLBACK <id>`; anything else cancels. If you confirm, the restore is applied atomically as a **new** revision (history is never rewritten). If the change is committed but the confirmation line cannot be written (for example the terminal was closed), memb says the rollback was applied and exits 1; run `memb revisions` to see it.
- **Only single-note updates can be rolled back.** Creations, supersessions, retractions and anything that touched several notes are refused, and the command prints the reason (`memb: rollback failed: <reason>`, for example "only single-note updates can be planned; grouped and lifecycle operations require store support" for a creation, supersession or retraction, "revision is stale or current note is missing" after the note changed, or "revision not found"); undo those with the normal tools instead: retract a note you created by mistake, and create a new note to replace one that was retracted or superseded (`supersede` only works on an active claim). A rollback is also refused if the note changed after that revision, if the restored text no longer passes validation, or if its parents no longer exist. A revision can be rolled back once: after that, the note no longer matches its after-image.
