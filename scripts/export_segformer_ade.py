#!/usr/bin/env python3
"""Export the SegFormer-B0 model fine-tuned on ADE20K (150 classes,
including sky, water, sea, boat, etc.) to ONNX for Bris.

The HuggingFace model `nvidia/segformer-b0-finetuned-ade-512-512` is
the canonical small SegFormer trained on ADE20K. ~3.7M parameters,
~15 MB ONNX, ~150-200ms inference at 512x512 on x86_64.

ADE20K class indices we care about:
    2  sky
   21  water  (or sea — naming varies; use the upstream label map)
   26  sea
   28  road / earth
   76  boat
   ...

The full label map ships with the model config; we read it at
inference time from the embedded JSON.

Run from the repo root:

    /tmp/bris-venv/bin/python3 scripts/export_segformer_ade.py \\
        crates/bris-vision/data/segmentation.onnx \\
        crates/bris-vision/data/segmentation_labels.json
"""

import json
import sys
from pathlib import Path

import torch
from transformers import SegformerForSemanticSegmentation


def main() -> None:
    if len(sys.argv) != 3:
        print(
            "usage: export_segformer_ade.py <model.onnx> <labels.json>",
            file=sys.stderr,
        )
        sys.exit(2)
    model_path = Path(sys.argv[1])
    labels_path = Path(sys.argv[2])
    model_path.parent.mkdir(parents=True, exist_ok=True)

    name = "nvidia/segformer-b0-finetuned-ade-512-512"
    print(f"loading {name}", file=sys.stderr)
    model = SegformerForSemanticSegmentation.from_pretrained(name)
    model.eval()

    # The model accepts (1, 3, H, W) RGB float input, normalized to
    # ImageNet mean/std. We export at 512x512 as the model was
    # natively trained.
    h = w = 512
    dummy = torch.zeros(1, 3, h, w, dtype=torch.float32)

    class Wrapper(torch.nn.Module):
        def __init__(self, m: torch.nn.Module) -> None:
            super().__init__()
            self.m = m

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            # SegFormer's output is (1, num_classes, H/4, W/4) by
            # default — the last upsample is left to the user. We
            # explicitly upsample to the input resolution here so the
            # ONNX consumer doesn't need to do it.
            logits = self.m(pixel_values=x).logits
            return torch.nn.functional.interpolate(
                logits, size=(h, w), mode="bilinear", align_corners=False
            )

    wrapped = Wrapper(model).eval()

    print(f"exporting to {model_path}", file=sys.stderr)
    torch.onnx.export(
        wrapped,
        (dummy,),
        str(model_path),
        input_names=["input"],
        output_names=["logits"],
        opset_version=13,
        dynamo=False,
    )

    # Save the label map: id → name.
    id2label = {int(k): v for k, v in model.config.id2label.items()}
    with open(labels_path, "w") as f:
        json.dump(id2label, f, indent=2)

    size_mb = model_path.stat().st_size / (1024 * 1024)
    print(f"done: {model_path} ({size_mb:.1f} MB)", file=sys.stderr)
    print(f"      {labels_path}", file=sys.stderr)
    # Highlight the few labels we care about.
    interesting = ["sky", "water", "sea", "boat", "ship", "person", "tree"]
    for idx, name in id2label.items():
        if any(t in name for t in interesting):
            print(f"  class {idx:3}: {name}", file=sys.stderr)


if __name__ == "__main__":
    main()
