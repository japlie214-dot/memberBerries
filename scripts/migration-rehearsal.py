"""Cross-version migration rehearsal: a v0.3.0 binary writes, the v0.4 binary migrates a COPY.

Usage: python scripts/migration-rehearsal.py <v0.3.0 memb.exe> <v0.4 memb.exe> <empty scratch dir>
Build the old binary from git commit e40d8be (v0.3.0) in a separate worktree. Never point this at a live data directory.
"""
import json, os, shutil, subprocess, sys
from pathlib import Path

OLD, NEW, S = (Path(a).resolve() for a in sys.argv[1:4])
S.mkdir(parents=True, exist_ok=True)
assert not any(S.iterdir()), "scratch dir must be empty"
live = S / "rehearsal-live"
shutil.rmtree(live, ignore_errors=True)
live.mkdir()


def call(exe, data, frames_tail):
    frames = [
        {"id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "r", "version": "1"}}},
        {"method": "notifications/initialized"},
    ] + frames_tail
    req = "".join(json.dumps({"jsonrpc": "2.0", **f}) + "\n" for f in frames)
    run = subprocess.run([str(exe), "mcp"], input=req, capture_output=True, text=True, timeout=60,
                         env={**os.environ, "MEMB_DATA_DIR": str(data)})
    out = [json.loads(l) for l in run.stdout.splitlines()]
    return out[1:], run.stderr, run.returncode


def tool(i, name, args):
    return {"id": i, "method": "tools/call", "params": {"name": name, "arguments": args}}


def text(r):
    assert not r["result"]["isError"], r
    return json.loads(r["result"]["content"][0]["text"])


# 1. Populate with the real v0.3.0 binary: two projects, full graph in alpha.
def old_write(project, items):
    ids = {}
    for kind, title, body, parents in items:
        pr = [ids[p] for p in parents]
        out, err, rc = call(OLD, live, [tool(2, "remember", {"project": project, "title": title, "body": body, "kind": kind, "parents": pr, "confirmed": True})])
        note = text(out[0])["note"]
        ids[title] = note["id"]
    return ids

alpha = old_write("alpha", [("topic", "Orchard", "Fruit trees", []), ("knowledge", "Irrigation", "Water schedule", ["Orchard"]), ("claim", "Water on Fridays", "Hose check every Friday", ["Irrigation"])])
beta = old_write("beta", [("topic", "Garage", "Tools and bikes", []), ("topic", "Kitchen", "Appliances", [])])
print("v0.3.0 wrote:", alpha, beta)

# 2. Backup (verified by hash), then migrate the COPY only.
backup = S / "rehearsal-backup"
shutil.rmtree(backup, ignore_errors=True)
shutil.copytree(live, backup)
import hashlib
h = lambda p: hashlib.sha256((p / "memory.redb").read_bytes()).hexdigest()
assert h(live) == h(backup)
copy = S / "rehearsal-copy"
shutil.rmtree(copy, ignore_errors=True)
shutil.copytree(backup, copy)

# 3. New binary opens the copy: counts and content preserved?
out, err, rc = call(NEW, copy, [tool(2, "search", {"query": "orchard"}), tool(3, "search", {"query": "garage"}), tool(4, "search", {"query": "friday"})])
print("stderr:", repr(err), "rc:", rc)
found = {}
for o, q in zip(out, ("orchard", "garage", "friday")):
    notes = text(o)["notes"]
    found[q] = [(n["kind"], n["title"], n["desc"]) for n in notes]
print(json.dumps(found, indent=1))
assert any(t == "Orchard" for _, t, _ in found["orchard"])
assert ("topic", "Garage", "Tools and bikes") in found["garage"]
assert any(t == "Water on Fridays" and d == "Hose check every Friday" for _, t, d in found["friday"])

# 4. Every legacy note present exactly once (6 expected: 3 alpha + 2 beta = 5).
allq = set()
for q in ("orchard", "irrigation", "water", "garage", "kitchen"):
    out, _, _ = call(NEW, copy, [tool(2, "search", {"query": q})])
    allq |= {(n["title"]) for n in text(out[0])["notes"]}
print("titles after migration:", sorted(allq))
assert allq == {"Orchard", "Irrigation", "Water on Fridays", "Garage", "Kitchen"}, allq

# 5. The backup is byte-identical (the rehearsal never touched it) and the live copy was untouched.
assert h(backup) == h(live)
print("backup/live untouched; hash", h(live)[:16])

# 6. Old binary must be refused/diverge-safe on the migrated copy? The new store must refuse if legacy table is changed afterwards.
out, err, rc = call(OLD, copy, [tool(2, "remember", {"project": "alpha", "title": "Late", "body": "written by old binary", "kind": "topic", "parents": [], "confirmed": True})])
print("old binary write on migrated copy:", "ok" if "result" in out[0] and not out[0]["result"].get("isError") else out[0])
out, err, rc = call(NEW, copy, [tool(2, "search", {"query": "late"})])
refused = rc != 0 or not out or "error" in out[0] or out[0].get("result", {}).get("isError")
print("new binary after old-binary write ->", "REFUSED" if refused else "ACCEPTED", "| stderr:", repr(err[:120]))
assert refused, "v0.4 store must refuse a database that an old client wrote after migration"
print("REHEARSAL DONE")
