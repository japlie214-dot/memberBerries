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

## Fix found while proving restart (0.13.1, 2026-11-27)

First attempt at "kill the service, next search restarts it" measured **905 s** for a search whose output was captured (`& memb search ... | Out-Null`): the service was started with the caller's inherited pipe handles, so the caller waited for it to exit (900 s idle). 0.13.1 starts it with `CreateProcessW` and no inherited handles. Same test on 0.13.1, output captured:

| Step | Time |
|---|---|
| Search, service cold (starts it, loads model) | 15.6 s |
| Next search | 0.41 s |
| Service killed with Stop-Process, then search | 16.4 s, new service pid |
| `memb hook prompt-submit`, service running / not running, output captured | 0.12 s / 0.21 s |
| Service working set | 192 MB |
