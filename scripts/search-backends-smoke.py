"""Gate C smoke: `memb search` works with and without an embedding backend.

Usage: python -I scripts/search-backends-smoke.py <memb.exe> <empty scratch dir> <gemma2 onnx dir> [llama-server.exe gguf]

Only synthetic text is stored. The query "bright red apple" shares no word with the note
("scarlet apples" / "fresh fruit"), so only a vector backend can find it; lexical search must return nothing.
"""
import json
import os
import socket
import subprocess
import sys
from pathlib import Path

exe, scratch, gemma = (Path(a).resolve() for a in sys.argv[1:4])
server = [Path(a).resolve() for a in sys.argv[4:6]]
scratch.mkdir(parents=True, exist_ok=True)
assert not any(scratch.iterdir()), "scratch dir must be empty"
data = scratch / "data"
BASE = {k: v for k, v in os.environ.items() if not k.startswith("MEMB_")}
BASE["MEMB_DATA_DIR"] = str(data)


def remember():
    frames = [
        {"id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "s", "version": "1"}}},
        {"method": "notifications/initialized"},
        {"id": 2, "method": "tools/call", "params": {"name": "remember", "arguments": {"title": "scarlet apples", "desc": "fresh fruit", "kind": "topic", "parents": [], "confirmed": True}}},
    ]
    req = "".join(json.dumps({"jsonrpc": "2.0", **f}) + "\n" for f in frames)
    run = subprocess.run([str(exe), "mcp"], input=req, capture_output=True, text=True, timeout=60, env=BASE, check=True)
    assert not json.loads(run.stdout.splitlines()[1])["result"]["isError"]


def search(label, extra, query="bright red apple"):
    run = subprocess.run([str(exe), "search", query], capture_output=True, text=True, timeout=240, env={**BASE, **extra})
    hit = "scarlet apples" in run.stdout
    print(f"{label:<44} exit={run.returncode} found_note={hit}")
    return run.returncode, hit


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


remember()
# 1. No backend: lexical only. The vector-only query finds nothing, a lexical query finds the note, nothing fails.
assert search("no backend, vector-only query", {}) == (0, False)
assert search("no backend, lexical query", {}, "apples") == (0, True)
# 2. In-process EmbeddingGemma 2, full width and Matryoshka 256.
assert search("EmbeddingGemma 2 (768)", {"MEMB_GEMMA_MODEL_DIR": str(gemma)}) == (0, True)
assert search("EmbeddingGemma 2 (MRL 256)", {"MEMB_GEMMA_MODEL_DIR": str(gemma), "MEMB_EMBED_MRL_DIM": "256"}) == (0, True)
# 3. Unsupported MRL size or a missing model: degrade to lexical, never fail.
assert search("EmbeddingGemma 2, unsupported MRL 999", {"MEMB_GEMMA_MODEL_DIR": str(gemma), "MEMB_EMBED_MRL_DIM": "999"}) == (0, False)
assert search("missing model dir, lexical query", {"MEMB_GEMMA_MODEL_DIR": str(scratch / "nope")}, "apples") == (0, True)
# 4. Loopback server configured but not running: degrade to lexical.
assert search("dead loopback port, lexical query", {"MEMB_EMBED_PORT": str(free_port()), "MEMB_EMBED_MODEL_ID": "dead"}, "apples") == (0, True)
# 5. Optional real llama-server (300M GGUF), only when its paths are supplied.
if len(server) == 2:
    port = free_port()
    child = subprocess.Popen([str(server[0]), "-m", str(server[1]), "--embeddings", "--host", "127.0.0.1", "--port", str(port), "--pooling", "mean"],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        import time, urllib.request
        for _ in range(120):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1)
                break
            except Exception:
                time.sleep(1)
        assert search("llama-server 300M GGUF over loopback", {"MEMB_EMBED_PORT": str(port), "MEMB_EMBED_MODEL_ID": "gemma300m-q8"}) == (0, True)
    finally:
        child.kill()
        child.wait()
print("PASS: search degrades to lexical without a backend and finds vector-only matches with one")
