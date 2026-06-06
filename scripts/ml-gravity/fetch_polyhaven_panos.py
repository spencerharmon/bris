#!/usr/bin/env python3
"""Fetch CC0 panoramas from Polyhaven for synthesized-tilt training.

Polyhaven assets are all CC0 (https://polyhaven.com/license).
We fetch the 2k JPG ldr versions (~1-3MB each); we only need ldr
because the perspective extractor renders 8-bit RGB.

Usage:
    python3 fetch_polyhaven_panos.py --out data/polyhaven --count 60
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import requests

API = "https://api.polyhaven.com"


def list_hdris() -> list[str]:
    r = requests.get(f"{API}/assets?type=hdris", timeout=30)
    r.raise_for_status()
    # Returned as a dict {slug: {...}}; preserve insertion order.
    return list(r.json().keys())


def fetch_pano(slug: str, out_dir: Path) -> Path | None:
    out = out_dir / f"{slug}.jpg"
    if out.exists() and out.stat().st_size > 100_000:
        with out.open("rb") as f:
            head = f.read(3)
        if head == b"\xff\xd8\xff":
            return out
        out.unlink()
    # The tonemapped JPG is the canonical ldr panorama; it's
    # larger than we strictly need (often 20-50 MB) but it's
    # the only ldr asset Polyhaven exposes.
    try:
        r = requests.get(f"{API}/files/{slug}", timeout=30)
        r.raise_for_status()
        tone = r.json().get("tonemapped")
        if not (isinstance(tone, dict) and "url" in tone):
            print(f"  no tonemapped jpg for {slug}", file=sys.stderr)
            return None
        url = tone["url"]
        with requests.get(url, stream=True, timeout=120) as resp:
            resp.raise_for_status()
            tmp = out.with_suffix(out.suffix + ".part")
            with tmp.open("wb") as f:
                for chunk in resp.iter_content(1 << 16):
                    f.write(chunk)
            # Shrink to a manageable size for training.
            from PIL import Image as _Image
            img = _Image.open(tmp).convert("RGB")
            # Aspect 2:1 equirectangular; downsample to 2048x1024.
            img = img.resize((2048, 1024), _Image.LANCZOS)
            img.save(out, "JPEG", quality=88)
            tmp.unlink(missing_ok=True)
        return out
    except Exception as e:
        print(f"  fetch failed for {slug}: {e}", file=sys.stderr)
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--count", type=int, default=60)
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    slugs = list_hdris()
    print(f"polyhaven catalog: {len(slugs)} hdris; fetching up to {args.count}")
    fetched = 0
    for slug in slugs:
        if fetched >= args.count:
            break
        path = fetch_pano(slug, args.out)
        if path:
            print(f"  ok  {slug} ({path.stat().st_size // 1024} KiB)")
            fetched += 1
            time.sleep(0.2)
    manifest = args.out / "manifest.json"
    manifest.write_text(json.dumps(
        {"source": "polyhaven.com", "license": "CC0", "count": fetched}
    ))
    print(f"done: {fetched} panoramas under {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
