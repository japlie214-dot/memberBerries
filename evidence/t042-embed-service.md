# T042 shared embedding service: live check (memb 0.13.0 release build, 2026-11-26)

4-bit EmbeddingGemma 2 (256-wide) installed with `memb models install --variant q4`. 12 cores, 16 GB.

| Step | Time |
|---|---|
| `memb embed-service status` before any search | not running |
| First `memb search` (starts the service, loads the model, indexes the notes) | 10.0 s |
| Next `memb search` calls, each a new process using the running service | 0.50 s and 0.55 s |
| `memb hook prompt-submit` with the service running | 0.41 s |
| Service working set after searches | about 198 MB (one copy, shared by every caller) |
| `memb embed-service stop`, then `status` | not running |

Before T042 each hook and each host server loaded its own model (3 to 6 s per hook, 165 MB or more each).
Note: `status` called within 200 ms of `stop` can still answer; the service exits a moment after replying.
