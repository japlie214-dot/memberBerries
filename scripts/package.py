"""Build Windows ZIP-based host packages from the locked Rust release binary."""
import hashlib
import json
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LAYOUTS = {
    "memb.plugin": ("cowork", (".claude-plugin/plugin.json", ".mcp.json", "skills/memb/SKILL.md")),
    "memb-agy.zip": ("agy", ("plugin.json", "mcp_config.json")),
    "memb-codex.zip": ("codex", (".codex-plugin/plugin.json", ".mcp.json")),
}


def main():
    exe = (ROOT / "target/release/memb.exe").read_bytes()
    if not exe.startswith(b"MZ"):
        raise ValueError("expected a Windows executable")
    out = ROOT / "dist"
    out.mkdir(exist_ok=True)
    checksums = []
    for filename, (host, paths) in LAYOUTS.items():
        entries = {f: (ROOT / "packages" / host / f).read_bytes() for f in paths}
        entries["server/memb.exe"] = exe
        for name, data in entries.items():
            if name.endswith(".json"):
                json.loads(data)
        package = out / filename
        with zipfile.ZipFile(package, "w") as archive:
            for name, data in sorted(entries.items()):
                archive.writestr(zipfile.ZipInfo(name), data, compress_type=zipfile.ZIP_DEFLATED)
        with zipfile.ZipFile(package) as archive:
            assert set(archive.namelist()) == set(entries) and archive.testzip() is None
        checksums.append(f"{hashlib.sha256(package.read_bytes()).hexdigest()}  {filename}")
        print(package.name)
    (out / "SHA256SUMS.txt").write_text("\n".join(checksums) + "\n", encoding="ascii", newline="\n")


if __name__ == "__main__":
    main()
