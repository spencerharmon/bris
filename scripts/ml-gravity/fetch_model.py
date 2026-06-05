#!/usr/bin/env python3
"""Fetch the heteroscedastic ML-gravity ONNX from the
release URL recorded in `data/ml-gravity/MODEL_URL`.

Per operator handoff 2026-06-05 (commit 0b2c306):
    B2 = fetch-at-build with checksum.

Usage:
    python3 scripts/ml-gravity/fetch_model.py [--out PATH]

Reads:
    data/ml-gravity/MODEL_URL    one line: download URL
    data/ml-gravity/SHA256SUMS   single line: '<hex>  <filename>'

Writes:
    data/ml-gravity/geocalib-heteroscedastic-v1.onnx   (or --out)

Idempotent: returns exit 0 + 'already present' when the file
on disk matches the recorded checksum.
"""
from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parent.parent.parent
DEFAULT_DIR = ROOT / "data" / "ml-gravity"


def parse_sums(path: Path) -> dict[str, str]:
    out = {}
    if not path.exists():
        return out
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) >= 2:
            out[parts[1].lstrip("*")] = parts[0]
    return out


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=None,
                    help="Output path (default data/ml-gravity/<filename>)")
    args = ap.parse_args()

    url_file = DEFAULT_DIR / "MODEL_URL"
    sums_file = DEFAULT_DIR / "SHA256SUMS"
    if not url_file.exists():
        print(f"missing {url_file} \u2014 nothing to fetch", file=sys.stderr)
        return 1
    if not sums_file.exists():
        print(f"missing {sums_file} \u2014 cannot verify", file=sys.stderr)
        return 1

    url = url_file.read_text().strip()
    sums = parse_sums(sums_file)
    if not sums:
        print("SHA256SUMS empty", file=sys.stderr)
        return 1
    filename, want_hash = next(iter(sums.items()))
    out = args.out or (DEFAULT_DIR / filename)
    out.parent.mkdir(parents=True, exist_ok=True)

    if out.exists():
        have = sha256(out)
        if have == want_hash:
            print(f"already present: {out} (sha256 {have[:12]}\u2026)")
            return 0
        print(f"checksum mismatch \u2014 refetching ({out})", file=sys.stderr)

    print(f"fetching {url}")
    tmp = out.with_suffix(out.suffix + ".part")
    with urlopen(url) as resp, tmp.open("wb") as f:
        while True:
            chunk = resp.read(1 << 16)
            if not chunk:
                break
            f.write(chunk)
    have = sha256(tmp)
    if have != want_hash:
        tmp.unlink()
        print(f"checksum failure: want {want_hash}, got {have}", file=sys.stderr)
        return 2
    tmp.rename(out)
    print(f"wrote {out} ({out.stat().st_size} bytes, sha256 {have[:12]}\u2026)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
