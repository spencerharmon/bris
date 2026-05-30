#!/usr/bin/env python3
"""Synthesize a schema_version=1 bundle.json for legacy Android
debug-capture exports (index.jsonl + frames/, no bundle.json)."""
import json, subprocess, sys
from pathlib import Path

GPS_LAT = 30.148765807545743
GPS_LON = -97.84322098798616

# Cat S62 Pro factory profile from
# bris-android/.../engine/FactoryCalibration.kt
INTRINSICS = {
    "source": {"kind": "factory"},
    "profile_key": {
        "model": "S62 Pro", "lens_id": "0",
        "width": 4032, "height": 3024,
    },
    "width": 4032, "height": 3024,
    "fx": 3103.4061281557006, "fy": 3090.496744366685,
    "cx": 2013.857097640865,  "cy": 1491.4983945221607,
    "distortion": {
        "model": "brown_conrady",
        "k1": 0.02287385685683836,
        "k2": -0.027249189121853052,
        "k3": 0.0,
        "p1": -0.0020285902622051532,
        "p2": -0.004038950067724464,
    },
    "rms_px": 0.7331791456580863,
}

DEVICE = {
    "model": "S62 Pro",
    "os": "Android 11",
    "app_version": "0.1.0",
}

def b3(p):
    return subprocess.check_output(["b3sum", str(p)]).split()[0].decode()

def synth(bundle_dir: Path):
    index = bundle_dir / "index.jsonl"
    lines = [json.loads(l) for l in index.read_text().splitlines() if l.strip()]
    first, last = lines[0], lines[-1]
    first_pgm = bundle_dir / "frames" / f"{first['seq']:012d}.pgm"
    manifest = {
        "schema_version": 1,
        "bundle_id": bundle_dir.name,
        "device": DEVICE,
        "capture": {
            "source_rotation_deg": 270,
            "frame_count": len(lines),
            "started_unix_ms": first["captured_unix_ms"],
            "ended_unix_ms": last["captured_unix_ms"],
            "first_frame_blake3": b3(first_pgm),
        },
        "intrinsics": INTRINSICS,
        "gps_truth": {
            "lat": GPS_LAT, "lon": GPS_LON,
            "lat_sigma_m": 5.0, "lon_sigma_m": 5.0,
            "captured_unix_ms": first["captured_unix_ms"],
            "source": "operator_supplied_post_hoc",
        },
        "notes": "Synthesized post-hoc; Android writer is TODO per docs/design/debug_bundle_schema.md.",
    }
    out = bundle_dir / "bundle.json"
    out.write_text(json.dumps(manifest, indent=2))
    print(f"wrote {out}  ({len(lines)} frames)")

if __name__ == "__main__":
    for p in sys.argv[1:]:
        synth(Path(p))
