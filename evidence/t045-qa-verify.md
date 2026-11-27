# T045 QA evidence: score floor per embedding backend

Ticket: memberberries/release/T045
QA: kb-qa (verify stage). Workspace C:\Projects\memberBerries, branch build/memb, changes uncommitted (not committed by QA).

## Ponytail Diff
Files: crates/memb-rag/src/lib.rs, crates/memb/src/config.rs, crates/memb/tests/embed_bench.rs, config.example.toml, wiki/Configuration.md, wiki/Hooks.md, CHANGELOG.md, version bumps.

- Backend call sites (verified by grep): search_with_snowflake -> Backend::Snowflake (lib.rs:318); gemma search -> Backend::Gemma (lib.rs:624); search_with_server (loopback) -> Backend::Loopback (lib.rs:680); hybrid_search (library callers) -> Backend::Other (lib.rs:692). fuse() is the only caller of retrieval_params, and search_with_index passes its backend through to it (lib.rs:632-634, 760-769). No other fuse callers.
- Floor resolution: parse_params (lib.rs:746-756). A valid MEMB_RETRIEVAL_SCORE_FLOOR in (-1,1) wins; invalid or unset falls back to default_score_floor(backend). Correct for all four backends.
- Constants: GEMMA_SCORE_FLOOR 0.60 (lib.rs:715), SNOWFLAKE_SCORE_FLOOR 0.25 (lib.rs:720, present in the current file), UNMEASURED_SCORE_FLOOR 0.25 for Loopback and Other (lib.rs:727-732).
- Config (config.rs): score_floor is Option<f64>; Default is None (no longer pins 0.60). Validation runs only when Some (range 0.0 to 0.95). apply_retrieval_env exports MEMB_RETRIEVAL_SCORE_FLOOR only when Some, and the existing environment still wins (the is_none check). Behaviour for existing configs that set score_floor is unchanged (Some exports as before).
- Gemma behaviour unchanged (0.60 default). Only Snowflake and loopback/library defaults change, which is the intended defect fix.
- Minimality: the diff is proportionate. Validation is kept for explicit values; junk env falls back.

Advisory (not blocking):
- lib.rs:1701 contains `default_score_floor(Backend::Snowflake) < 0.239 + 0.05`, a hard-coded margin that reads oddly; the second clause carries the real check. Cosmetic.
- The new test exercises parse_params (the env-derived value's logic) rather than retrieval_params itself. The env read is a thin wrapper, so this is acceptable.
- Library callers using hybrid_search with supplied vectors move from 0.60 to 0.25. This follows the ticket (Other is unmeasured, permissive) and is documented in the evidence file, but it is a behaviour change for that caller group; mention it in the CHANGELOG if not already there.

## Testing Ladder
### T1 checkpoint
- Ticket Prove step T1 (recorded by the worker in the execution log): cargo test --workspace --locked --offline reported all ok. QA did not rerun the full workspace (per audit scope, about 6 minutes).
- QA reran the targeted suites in this audit:
  - `cargo test -p memb-rag --locked --offline`: 13 passed, 0 failed, 3 ignored (expected 13 passed, 3 ignored). Pass.
  - `cargo test -p memb --locked --offline --bin memb config`: 16 passed, 0 failed (expected 16). Pass.
- Checkpoint verdict: PASS.

### unit
- the_default_score_floor_follows_the_backend_and_an_explicit_setting_overrides_it: ok (positive: Gemma 0.60, Snowflake 0.25, unmeasured 0.25; explicit 0.33 wins for every backend; junk env falls back; 0.5 cosine dropped by Gemma and kept by Loopback).
- retrieval_settings_default_export_to_the_environment_and_never_override_it: ok (unset exports nothing; 0.99 refused; environment wins).
- defaults_match_the_kit_example, shipped_example_file_parses_to_the_defaults: ok.

### integration
n/a: the full workspace suite was not rerun (worker ran it, all ok); out of audit scope.

### component
n/a: no separate component layer for this change.

### e2e
n/a: the ignored benches were not run (local needs the model, Snowflake sends data externally). Recorded tables cited instead, see Positive + Pluto Witness.

## Positive + Pluto Witness
Positive: floor per backend and explicit override pass (unit test above).

Negative / witness:
- Mutation (lead, not repeated): SNOWFLAKE_SCORE_FLOOR 0.25 -> 0.60 makes the default-floor test fail at lib.rs:1701. QA confirmed the current file still has `pub const SNOWFLAKE_SCORE_FLOOR: f32 = 0.25;` (lib.rs:720) and sha256 prefix 7316f6acdcce3f28 (computed in this audit).
- Junk env and out-of-range floors fall back to the backend default (parse_params tests, lib.rs:1679 and 1698-1701).
- A 0.5 cosine vector is dropped by Gemma (0.60) and kept by Loopback (0.25) (lib.rs:1718-1719).

Evidence tables checked against raw files (evidence/t045-sweep-local.md and evidence/t045-sweep-snowflake.md):
- Local right note 0.617 / 0.741 / 0.839; off-topic best 0.543 / 0.607 / 0.622; wrong on-topic max 0.783. Match. Floor rows: 0.25 12/12 off-topic, recall 0.90; 0.55 11/12; 0.60 30/30 kept, 6/12 off-topic, recall 0.90; 0.625 29/30, 0/12; 0.70 20/30, recall 0.87. All match.
- Snowflake right note 0.239 / 0.403 / 0.577; off-topic best 0.125 / 0.216 / 0.260; wrong on-topic max 0.462. Match. Floor rows: 0.25 29/30, 1/12, recall 1.00; 0.42 11/30, 0.70; 0.50 3/30, 0.67; 0.60 0/30, 0/12, recall 0.63; 0.00 recall 0.93. All match.
- Loopback: honestly marked not measured in the evidence file (no llama-server or model on the machine), and kept at 0.25 (unmeasured fallback). Confirmed.

Recorded sweep tables were cited, not rerun (the ignored benches are out of scope). The Snowflake live run was approved by the user per the ticket log.

## Advisory Notes
- See Ponytail Diff advisories: cosmetic assert at lib.rs:1701; library hybrid_search behaviour change 0.60 -> 0.25 worth a CHANGELOG line.
- Evidence caveats are already stated in the evidence file (30 notes, one run per backend, synthetic corpus, a single Snowflake model).

## Verdict
PASS. Diff is correct for every search path, the explicit env floor beats the default, junk falls back, the unset config no longer pins 0.60 for all backends, existing configs that set score_floor behave as before, and the evidence tables match the raw sweep output. No P0/P1 findings.
