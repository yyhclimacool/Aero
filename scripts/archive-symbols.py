#!/usr/bin/env python3
"""Archive developer-only symbols and exact Mach-O images outside the distributed app."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def uuids(path):
    output = subprocess.check_output(["dwarfdump", "--uuid", str(path)], text=True)
    return [{"uuid": ident.upper(), "arch": arch} for ident, arch in
            re.findall(r"UUID: ([0-9a-fA-F-]{36}) \(([^)]+)\)", output)]


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    executable = args.app / "Contents/MacOS/aero"
    native = args.app / "Contents/Frameworks/libzvec_c_api.dylib"
    symbols = Path(str(args.binary) + ".dSYM")
    if not symbols.is_dir():
        subprocess.run(["dsymutil", str(args.binary), "-o", str(symbols)], check=True)
    identity = uuids(executable)
    if not identity or uuids(symbols) != identity:
        raise SystemExit("Aero dSYM UUID does not match the packaged executable.")
    sha = digest(executable)
    args.output.mkdir(parents=True, exist_ok=True)
    destination = args.output / (identity[0]["uuid"] + "-" + sha[:12])
    if destination.exists():
        manifest = json.loads((destination / "manifest.json").read_text())
        if manifest["sha256"] != sha:
            raise SystemExit("Existing symbol archive does not match the build.")
        print(f"Symbols: {destination}")
        return
    temporary = Path(tempfile.mkdtemp(prefix=".symbols-", dir=args.output))
    try:
        shutil.copytree(symbols, temporary / "aero.dSYM")
        shutil.copy2(executable, temporary / "aero")
        shutil.copy2(native, temporary / native.name)
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        dirty = subprocess.run(["git", "diff", "--quiet", "HEAD"], check=False).returncode != 0
        manifest = {"schema": 1, "version": args.version, "commit": commit, "dirty": dirty,
                    "binary": identity, "sha256": sha, "native": uuids(native),
                    "native_sha256": digest(native), "native_debug_symbols": False}
        (temporary / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        temporary.rename(destination)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    print(f"Symbols: {destination}")


if __name__ == "__main__":
    main()
