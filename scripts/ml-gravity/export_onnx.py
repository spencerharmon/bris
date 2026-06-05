#!/usr/bin/env python3
"""Export trained heteroscedastic-head model to ONNX.

The exported ONNX has:
    input  : NCHW float32, shape (1, 3, 256, 256), ImageNet-normalized.
    output : (1, 4) float32  = [roll, pitch, log_var_roll, log_var_pitch].

After export we run onnxsim to fold constants and onnxruntime to
verify finite outputs on a zero tensor.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import torch
from onnxsim import simplify

sys.path.insert(0, str(Path(__file__).parent))
from train_heteroscedastic import HeteroscedasticHead, INPUT_SIZE  # noqa: E402


def export(weights: Path, out: Path) -> None:
    model = HeteroscedasticHead()
    state = torch.load(weights, map_location="cpu", weights_only=True)
    model.load_state_dict(state)
    model.eval()

    dummy = torch.zeros(1, 3, INPUT_SIZE, INPUT_SIZE, dtype=torch.float32)
    out.parent.mkdir(parents=True, exist_ok=True)
    raw = out.with_suffix(".raw.onnx")
    torch.onnx.export(
        model, dummy, raw,
        input_names=["image"], output_names=["roll_pitch_logvar"],
        opset_version=17,
        dynamic_axes=None,  # fixed 1x3x256x256
    )
    proto = onnx.load(raw)
    simp, ok = simplify(proto)
    if not ok:
        print("onnxsim could not validate; keeping raw graph", file=sys.stderr)
        simp = proto
    onnx.save(simp, out)
    raw.unlink(missing_ok=True)

    # Smoke test in onnxruntime.
    sess = ort.InferenceSession(str(out), providers=["CPUExecutionProvider"])
    y = sess.run(None, {"image": dummy.numpy()})[0]
    assert y.shape == (1, 4), f"unexpected output shape {y.shape}"
    assert np.isfinite(y).all(), f"non-finite output: {y}"
    print(f"onnx export ok, sample output: {y.tolist()}")

    digest = hashlib.sha256(out.read_bytes()).hexdigest()
    blake3 = None
    try:
        import blake3 as b3
        blake3 = b3.blake3(out.read_bytes()).hexdigest()
    except ImportError:
        pass
    size = out.stat().st_size
    (out.parent / "export_info.json").write_text(json.dumps({
        "file": out.name,
        "size_bytes": size,
        "sha256": digest,
        "blake3": blake3,
    }, indent=2))
    print(f"export size {size} bytes  sha256 {digest}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--weights", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()
    export(args.weights, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
