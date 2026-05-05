#!/usr/bin/env python3
"""Export torchvision's DeepLabv3-MobileNetV3-Large to ONNX for use
by Bris's segmentation-based horizon detector.

DeepLabv3-MobileNetV3 is the smallest practical pretrained semantic
segmentation model in torchvision. It's trained on COCO + a subset of
Pascal VOC and outputs 21 classes including:
    0  background
    4  boat
    9  chair
   ... etc.

For Bris's purpose we treat any non-(boat|background) prediction with
high confidence on the upper half of the frame as "sky/sky-like," and
distinguish "boat" pixels separately so the horizon detector can skip
columns occluded by the vessel.

We deliberately use MobileNetV3 (not the full ResNet) backbone for two
reasons:
  1. ONNX export is clean — no transformer ops that tract may not
     support.
  2. ~5 MB ONNX file, ~50-200ms inference on a Pi Zero 2W.

Run from the repo root:

    /tmp/bris-venv/bin/python3 scripts/export_segmentation_model.py \\
        crates/bris-vision/data/segformer_b0_ade.onnx

(The output filename uses 'segformer_b0_ade' for symmetry with the
public model zoo naming convention but the actual model is
DeepLabv3-MobileNetV3-Large. We may swap to a real SegFormer later if
tract gains support; the file path is stable so the loader doesn't
need to change.)
"""

import sys
from pathlib import Path

import torch
from torchvision.models.segmentation import (
    deeplabv3_mobilenet_v3_large,
    DeepLabV3_MobileNet_V3_Large_Weights,
)


def main() -> None:
    if len(sys.argv) != 2:
        print("usage: export_segmentation_model.py <output.onnx>", file=sys.stderr)
        sys.exit(2)
    out_path = Path(sys.argv[1])
    out_path.parent.mkdir(parents=True, exist_ok=True)

    weights = DeepLabV3_MobileNet_V3_Large_Weights.COCO_WITH_VOC_LABELS_V1
    model = deeplabv3_mobilenet_v3_large(weights=weights)
    model.eval()

    # Dummy input at the resolution we'll run inference at. The model is
    # fully convolutional so the resolution can change at runtime, but
    # tract benefits from a fixed shape. 256x256 is a good speed/quality
    # tradeoff for our use.
    dummy = torch.zeros(1, 3, 256, 256, dtype=torch.float32)

    # The model returns a dict {"out": logits}; ONNX export needs a
    # plain tensor output, so wrap it.
    class Wrapper(torch.nn.Module):
        def __init__(self, m: torch.nn.Module) -> None:
            super().__init__()
            self.m = m

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            return self.m(x)["out"]

    wrapped = Wrapper(model).eval()

    print(f"exporting to {out_path}", file=sys.stderr)
    torch.onnx.export(
        wrapped,
        (dummy,),
        str(out_path),
        input_names=["input"],
        output_names=["logits"],
        opset_version=13,
        dynamo=False,
    )

    size_mb = out_path.stat().st_size / (1024 * 1024)
    print(f"done: {out_path} ({size_mb:.1f} MB)", file=sys.stderr)
    print(
        "Class indices (Pascal VOC): "
        "0=background, 1=aeroplane, 2=bicycle, 3=bird, 4=boat, "
        "5=bottle, 6=bus, 7=car, 8=cat, 9=chair, 10=cow, 11=diningtable, "
        "12=dog, 13=horse, 14=motorbike, 15=person, 16=pottedplant, "
        "17=sheep, 18=sofa, 19=train, 20=tvmonitor",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
