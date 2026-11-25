"""Verify packaged executables exchange notes without touching live profiles."""
import hashlib
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
