"""Verify packaged executables exchange notes without touching live profiles."""
import hashlib
import shutil
import json
import os
import subprocess
import tempfile
import zipfile
from pathlib import Path

from sys import argv

ROOT = Path(__file__).resolve().parents[1]
EXPECTED_TOOLS = {"remember", "search", "inspect", "update", "supersede", "retract", "curator_evidence", "curator_preview", "curator_apply", "scroll_chunk_read"}


def call(exe, store, tool, args):
    frames = [
        {"id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "archive-smoke", "version": "1"}}},
        {"method": "notifications/initialized"},
        {"id": 2, "method": "tools/list"},
        {"id": 3, "method": "tools/call", "params": {"name": tool, "arguments": args}},
    ]
    request = "".join(json.dumps({"jsonrpc": "2.0", **f}) + "\n" for f in frames)
    run = subprocess.run([exe, "mcp"], input=request, capture_output=True, text=True, timeout=20, check=True,
                         env={**os.environ, "MEMB_DATA_DIR": str(store)})
    assert not run.stderr, run.stderr
    init, tools, response = map(json.loads, run.stdout.splitlines())
    assert init["result"]["protocolVersion"] == "2025-06-18"
    assert {t["name"] for t in tools["result"]["tools"]} == EXPECTED_TOOLS
    assert not response["result"]["isError"], response
    return json.loads(response["result"]["content"][0]["text"])


HOOK_FILES = {"memb.plugin": "hooks/hooks.json", "memb-codex.zip": "hooks/hooks.json", "memb-agy.zip": "hooks.json"}


def hook_commands(name, archive):
    """Every command string a package's hook file declares, as the host would run it."""
    data = json.loads(archive.read(HOOK_FILES[name]))
    if name == "memb-agy.zip":  # {"<group>": {"Stop": [{...}]}}
        groups = [h for group in data.values() for event in group.values() if isinstance(event, list) for h in event]
        events = {e for group in data.values() for e in group if e != "enabled"}
    else:  # {"hooks": {"<Event>": [{"hooks": [{...}]}]}}
        groups = [h for matcher in (m for v in data["hooks"].values() for m in v) for h in matcher["hooks"]]
        events = set(data["hooks"])
    return events, [" ".join([h["command"], *h.get("args", [])]) for h in groups]


def check_hooks(name, archive, exe, tmp):
    events, commands = hook_commands(name, archive)
    want = {"memb-agy.zip": {"Stop"}}.get(name, {"UserPromptSubmit", "Stop"})
    assert events == want, (name, events)
    assert commands and all("server" in c and "memb.exe" in c and " hook " in f" {c} " and f"--cli" in c for c in commands), commands
    # The packaged binary must treat a hook as a silent no-op unless capture is opted into.
    cwd = tmp / "hook-cwd"
    cwd.mkdir(exist_ok=True)
    payload = json.dumps({"session_id": "smoke", "cwd": str(cwd), "prompt": "hello", "last_assistant_message": "hi"})
    data = tmp / f"hook-data-{name}"
    env = {k: v for k, v in os.environ.items() if not k.startswith("MEMB_")}
    for extra in ({}, {"MEMB_SCROLL_CAPTURE": "1"}):
        for event in ("prompt-submit", "assistant-stop"):
            run = subprocess.run([exe, "hook", event, "--cli", "cc"], input=payload, capture_output=True, text=True, timeout=30,
                                 env={**env, "MEMB_DATA_DIR": str(data), **extra}, cwd=cwd)
            assert run.returncode == 0 and run.stdout == "", (name, event, extra, run.returncode, run.stdout)
    return len(commands)


def main(temp_parent):
    dist = ROOT / "dist"
    with tempfile.TemporaryDirectory(prefix="memb package smoke ", dir=temp_parent) as tmp:
        tmp = Path(tmp)
        paths = {}
        for spec in (dist / "SHA256SUMS.txt").read_text(encoding="ascii").splitlines():
            digest, name = spec.split("  ")
            package = dist / name
            assert hashlib.sha256(package.read_bytes()).hexdigest() == digest
            with zipfile.ZipFile(package) as archive:
                assert archive.testzip() is None
                path = tmp / name / "memb.exe"
                path.parent.mkdir()
                path.write_bytes(archive.read("server/memb.exe"))
                paths[name] = path
                if name in HOOK_FILES:
                    print(f"hooks ok: {name} ({check_hooks(name, archive, path, tmp)} commands)")
                if name == "memb-agy.zip" and shutil.which("agy"):
                    check = subprocess.run(["agy", "plugin", "validate", str(package), "--strict"], capture_output=True, text=True, timeout=60)
                    assert check.returncode == 0, check.stdout + check.stderr
                    print("agy plugin validate --strict: ok")
        store = tmp / "data"
        note = call(paths["memb.plugin"], store, "remember", {"title": "Orchard", "desc": "Check the hose each Friday", "kind": "topic", "parents": [], "confirmed": True})["note"]
        for name in ("memb-agy.zip", "memb-codex.zip"):
            assert call(paths[name], store, "search", {"query": "orchard"})["notes"] == [note]
        updated = call(paths["memb-agy.zip"], store, "update", {"id": note["id"], "properties": {"checked": "yes"}, "confirmed": True})["note"]
        assert updated["properties"] == {"checked": "yes"}
        assert call(paths["memb-codex.zip"], store, "retract", {"id": note["id"], "confirmed": True})["retracted_root"] == note["id"]
        assert call(paths["memb.plugin"], store, "inspect", {"id": note["id"]})["note"]["status"] == "retracted"
        print("PASS: all three packaged binaries shared memory from separate stdio processes")


if __name__ == "__main__":
    main(argv[1] if len(argv) > 1 else os.environ.get("RUNNER_TEMP"))
