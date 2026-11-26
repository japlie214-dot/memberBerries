#!/usr/bin/env python3
"""Sets the memb version everywhere it is written, so one iteration is one number.

    python scripts/bump-version.py 0.6.0     # set an exact version
    python scripts/bump-version.py minor     # 0.5.3 -> 0.6.0  (a feature iteration)
    python scripts/bump-version.py patch     # 0.5.3 -> 0.5.4  (a fixes-only iteration)

Places updated: the workspace version in Cargo.toml, the four host manifests under
packages/, the lockfile (through cargo, offline), and the two sentences in the wiki that
name the current version. scripts/package.py refuses to package when the manifests and
the workspace disagree, and the CHANGELOG must have an entry for the new version.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFESTS = [
    "packages/agy/plugin.json",
    "packages/codex/.codex-plugin/plugin.json",
    "packages/cowork/.claude-plugin/plugin.json",
    "packages/desktop/manifest.json",
]


def current():
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    return re.search(r'^version = "(\d+)\.(\d+)\.(\d+)"', text, re.M).groups()


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    major, minor, patch = (int(x) for x in current())
    arg = sys.argv[1]
    if arg == "minor":
        new = f"{major}.{minor + 1}.0"
    elif arg == "patch":
        new = f"{major}.{minor}.{patch + 1}"
    elif re.fullmatch(r"\d+\.\d+\.\d+", arg):
        new = arg
    else:
        sys.exit(__doc__)
    old = f"{major}.{minor}.{patch}"
    if new == old:
        sys.exit(f"already {old}")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    if f"## {new}" not in changelog:
        sys.exit(f"CHANGELOG.md has no '## {new}' entry yet; write it first")

    def rewrite(path, fn):
        p = ROOT / path
        raw = p.read_bytes().decode("utf-8")
        crlf = "\r\n" in raw
        text = fn(raw.replace("\r\n", "\n"))
        p.write_bytes((text.replace("\n", "\r\n") if crlf else text).encode("utf-8"))

    rewrite("Cargo.toml", lambda t: re.sub(r'^version = "[^"]+"', f'version = "{new}"', t, count=1, flags=re.M))
    for manifest in MANIFESTS:
        rewrite(manifest, lambda t: re.sub(r'("version"\s*:\s*")[^"]+(")', rf"\g<1>{new}\g<2>", t, count=1))
        assert json.loads((ROOT / manifest).read_text(encoding="utf-8"))["version"] == new
    rewrite("wiki/Phase-Coverage.md", lambda t: re.sub(r"workspace \d+\.\d+\.\d+", f"workspace {new}", t))
    rewrite("wiki/Install.md", lambda t: re.sub(r"\bv\d+\.\d+\.\d+: `scripts/smoke-packages.py`", f"v{new}: `scripts/smoke-packages.py`", t))
    # Refresh the lockfile entries of the workspace crates without touching dependencies.
    subprocess.run(
        ["cargo", "update", "--workspace", "--offline"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    print(f"{old} -> {new}")


if __name__ == "__main__":
    main()
