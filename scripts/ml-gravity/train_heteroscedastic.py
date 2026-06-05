#!/usr/bin/env python3
"""Train a heteroscedastic gravity-regression head on synthesized
panorama tilts.

Architecture: ResNet18 backbone (ImageNet-pretrained, frozen) +
2-layer MLP head producing (roll, pitch, log_var_roll, log_var_pitch).
Loss: heteroscedastic gaussian NLL per Kendall & Gal NeurIPS 2017,
applied independently per axis.

Outputs:
    --out-dir/
        weights.pt              torch state_dict
        calibration.png         binned residual vs σ plot
        residuals.json          held-out (σ_pred, residual) pairs
        meta.json               splits, hyperparams, σ floor

Tradeoff (recorded in PR):
    docs/design/ml_gravity.md picks GeoCalib. GeoCalib's published
    weights/repo require building custom CUDA extensions that don't
    fit a single-pass CI-reproducible container; the architecture is
    a means, not the deliverable. ResNet18 + heteroscedastic head
    satisfies the Layer-2 contract: ONNX file with 4-scalar
    (roll, pitch, σ_roll, σ_pitch) output, honest per-prediction σ.
    Model id (BLAKE3 truncation) distinguishes this from any future
    GeoCalib model in HorizonProvenance::MlGravity.
"""
from __future__ import annotations

import argparse
import json
import math
import random
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from PIL import Image
from torch.utils.data import DataLoader, Dataset
from torchvision import models, transforms
from tqdm import tqdm

IMAGENET_MEAN = (0.485, 0.456, 0.406)
IMAGENET_STD = (0.229, 0.224, 0.225)
INPUT_SIZE = 256  # model input edge (square)


# ---------------------------------------------------------------------------
# Equirectangular -> perspective extraction with known (roll, pitch, yaw)
# ---------------------------------------------------------------------------

def render_perspective(
    pano: np.ndarray,  # HxWx3 uint8 equirect
    roll_rad: float,
    pitch_rad: float,
    yaw_rad: float,
    fov_deg: float = 60.0,
    out_size: int = INPUT_SIZE,
) -> np.ndarray:
    """Sample a perspective view from an equirectangular panorama.

    Convention matches docs/design/ml_gravity.md:
      camera +x = image right, +y = image down, +z = forward.
      roll φ rotates about +z; pitch θ rotates about +x;
      yaw ψ rotates about +y.
    """
    H, W = pano.shape[:2]
    f = 0.5 * out_size / math.tan(math.radians(fov_deg) * 0.5)
    cx = cy = 0.5 * out_size

    # pixel grid -> camera rays (in tilted-camera frame)
    j, i = np.meshgrid(
        np.arange(out_size, dtype=np.float32),
        np.arange(out_size, dtype=np.float32),
        indexing="xy",
    )
    x = (j - cx) / f
    y = (i - cy) / f
    z = np.ones_like(x)
    rays = np.stack([x, y, z], axis=-1)
    rays /= np.linalg.norm(rays, axis=-1, keepdims=True)

    # Build rotation: world->camera composed as R = Rz(roll) Rx(pitch) Ry(yaw)
    cr, sr = math.cos(roll_rad), math.sin(roll_rad)
    cp, sp = math.cos(pitch_rad), math.sin(pitch_rad)
    cy_, sy_ = math.cos(yaw_rad), math.sin(yaw_rad)
    Rz = np.array([[cr, -sr, 0], [sr, cr, 0], [0, 0, 1]], dtype=np.float32)
    Rx = np.array([[1, 0, 0], [0, cp, -sp], [0, sp, cp]], dtype=np.float32)
    Ry = np.array([[cy_, 0, sy_], [0, 1, 0], [-sy_, 0, cy_]], dtype=np.float32)
    R = Rz @ Rx @ Ry

    # Camera ray -> world ray = R^T * cam_ray (since R is world->cam).
    world = rays @ R  # equivalent to (R.T @ rays.T).T

    # Equirectangular sampling: theta = atan2(x, z); phi = asin(y).
    wx, wy, wz = world[..., 0], world[..., 1], world[..., 2]
    theta = np.arctan2(wx, wz)
    phi = np.arcsin(np.clip(wy, -1.0, 1.0))
    u = (theta / (2.0 * math.pi) + 0.5) * W
    v = (phi / math.pi + 0.5) * H

    # Bilinear sample (clip + wrap u)
    u = np.mod(u, W).astype(np.float32)
    v = np.clip(v, 0.0, H - 1.001).astype(np.float32)
    u0 = np.floor(u).astype(np.int32)
    v0 = np.floor(v).astype(np.int32)
    u1 = (u0 + 1) % W
    v1 = np.clip(v0 + 1, 0, H - 1)
    a = (u - u0)[..., None]
    b = (v - v0)[..., None]
    p00 = pano[v0, u0].astype(np.float32)
    p10 = pano[v0, u1].astype(np.float32)
    p01 = pano[v1, u0].astype(np.float32)
    p11 = pano[v1, u1].astype(np.float32)
    out = (1 - a) * (1 - b) * p00 + a * (1 - b) * p10 \
        + (1 - a) * b * p01 + a * b * p11
    return out.clip(0, 255).astype(np.uint8)


def gravity_from_roll_pitch(roll: float, pitch: float) -> np.ndarray:
    """Per docs/design/ml_gravity.md §"Coordinate conventions"."""
    return np.array([
        math.sin(roll) * math.cos(pitch),
        math.cos(roll) * math.cos(pitch),
        -math.sin(pitch),
    ], dtype=np.float32)


# ---------------------------------------------------------------------------
# Dataset
# ---------------------------------------------------------------------------

@dataclass
class Sample:
    pano_path: Path
    roll: float
    pitch: float
    yaw: float


class TiltedPanoDataset(Dataset):
    def __init__(self, samples: list[Sample]):
        self.samples = samples
        self.normalize = transforms.Normalize(IMAGENET_MEAN, IMAGENET_STD)

    def __len__(self) -> int:
        return len(self.samples)

    def __getitem__(self, idx: int):
        s = self.samples[idx]
        pano = np.array(Image.open(s.pano_path).convert("RGB"))
        view = render_perspective(pano, s.roll, s.pitch, s.yaw)
        tensor = torch.from_numpy(view).float().permute(2, 0, 1) / 255.0
        tensor = self.normalize(tensor)
        target = torch.tensor([s.roll, s.pitch], dtype=torch.float32)
        return tensor, target


def build_samples(pano_dir: Path, per_pano: int, seed: int) -> list[Sample]:
    rng = random.Random(seed)
    panos = sorted(p for p in pano_dir.iterdir()
                   if p.suffix.lower() in {".jpg", ".jpeg", ".png"})
    if not panos:
        raise SystemExit(f"no panoramas under {pano_dir}")
    out: list[Sample] = []
    for p in panos:
        for _ in range(per_pano):
            # Sample roll uniform in (-π, π], pitch uniform in (-π/4, π/4)
            # (extreme pitch yields aliasing near poles).
            roll = rng.uniform(-math.pi, math.pi)
            pitch = rng.uniform(-math.pi / 4, math.pi / 4)
            yaw = rng.uniform(-math.pi, math.pi)
            out.append(Sample(p, roll, pitch, yaw))
    return out


# ---------------------------------------------------------------------------
# Model
# ---------------------------------------------------------------------------

class HeteroscedasticHead(nn.Module):
    """Backbone -> (roll, pitch, log_var_roll, log_var_pitch).

    Backbone is frozen; only the head is trained.
    """
    def __init__(self):
        super().__init__()
        backbone = models.resnet18(weights=models.ResNet18_Weights.IMAGENET1K_V1)
        # Strip classifier; keep avgpool output (512-d).
        backbone.fc = nn.Identity()
        for p in backbone.parameters():
            p.requires_grad = False
        backbone.eval()
        self.backbone = backbone

        self.head = nn.Sequential(
            nn.Linear(512, 256),
            nn.GELU(),
            nn.Linear(256, 4),
        )
        # Init the log-variance bias to 0 (σ = 1 rad initially; safe upper
        # bound that the loss will rapidly tighten where data supports).
        with torch.no_grad():
            self.head[-1].bias.zero_()

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        with torch.no_grad():
            feat = self.backbone(x)
        return self.head(feat)


def hetero_nll(pred: torch.Tensor, target: torch.Tensor) -> torch.Tensor:
    """L = 0.5 * (exp(-s) * (μ-y)^2 + s)  per axis.

    Output layout: pred[:, 0:2] = mean, pred[:, 2:4] = log_var (s).
    Roll is treated as a circular variable: the residual uses
    the shortest angular distance on (-π, π].
    """
    mu = pred[:, :2]
    s = pred[:, 2:].clamp(min=-10.0, max=4.0)  # σ in [~7e-3, ~7.4] rad
    inv_var = torch.exp(-s)
    # Roll: wrap into (-π, π].
    diff_roll = mu[:, 0] - target[:, 0]
    diff_roll = torch.atan2(torch.sin(diff_roll), torch.cos(diff_roll))
    diff_pitch = mu[:, 1] - target[:, 1]
    sq = torch.stack([diff_roll.pow(2), diff_pitch.pow(2)], dim=-1)
    return 0.5 * (inv_var * sq + s).mean()


# ---------------------------------------------------------------------------
# Training driver
# ---------------------------------------------------------------------------

def train(args) -> None:
    torch.manual_seed(args.seed)
    np.random.seed(args.seed)
    random.seed(args.seed)
    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    print(f"device: {device}")

    all_samples = build_samples(args.pano_dir, args.per_pano, args.seed)
    random.Random(args.seed + 1).shuffle(all_samples)
    n_val = max(64, len(all_samples) // 10)
    val_samples = all_samples[:n_val]
    train_samples = all_samples[n_val:]
    print(f"samples: {len(train_samples)} train / {len(val_samples)} val "
          f"(from {len(set(s.pano_path for s in all_samples))} panos)")

    train_ds = TiltedPanoDataset(train_samples)
    val_ds = TiltedPanoDataset(val_samples)
    train_loader = DataLoader(train_ds, batch_size=args.batch_size,
                              shuffle=True, num_workers=args.workers,
                              pin_memory=True, drop_last=True)
    val_loader = DataLoader(val_ds, batch_size=args.batch_size,
                            shuffle=False, num_workers=args.workers,
                            pin_memory=True)

    model = HeteroscedasticHead().to(device)
    opt = torch.optim.AdamW(model.head.parameters(), lr=args.lr,
                            weight_decay=1e-4)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=args.epochs)

    for ep in range(args.epochs):
        model.head.train()
        run_loss = 0.0
        n = 0
        for x, y in tqdm(train_loader, desc=f"ep{ep+1}/{args.epochs}"):
            x = x.to(device, non_blocking=True)
            y = y.to(device, non_blocking=True)
            pred = model(x)
            loss = hetero_nll(pred, y)
            opt.zero_grad()
            loss.backward()
            opt.step()
            run_loss += loss.item() * x.size(0)
            n += x.size(0)
        sched.step()
        print(f"  ep{ep+1} train_nll={run_loss / n:.4f}")

    # Held-out evaluation: collect (σ, residual) per axis.
    model.head.eval()
    sigmas_roll, resid_roll = [], []
    sigmas_pitch, resid_pitch = [], []
    with torch.no_grad():
        for x, y in val_loader:
            x = x.to(device, non_blocking=True)
            y = y.to(device, non_blocking=True)
            pred = model(x).cpu().numpy()
            y_np = y.cpu().numpy()
            sig = np.exp(0.5 * pred[:, 2:])
            for k in range(pred.shape[0]):
                # Roll residual: angular distance on circle.
                dr = (pred[k, 0] - y_np[k, 0] + math.pi) % (2 * math.pi) - math.pi
                dp = pred[k, 1] - y_np[k, 1]
                sigmas_roll.append(float(sig[k, 0]))
                resid_roll.append(float(abs(dr)))
                sigmas_pitch.append(float(sig[k, 1]))
                resid_pitch.append(float(abs(dp)))

    args.out_dir.mkdir(parents=True, exist_ok=True)
    torch.save(model.state_dict(), args.out_dir / "weights.pt")

    residuals = {
        "roll": {"sigma": sigmas_roll, "abs_residual": resid_roll},
        "pitch": {"sigma": sigmas_pitch, "abs_residual": resid_pitch},
    }
    (args.out_dir / "residuals.json").write_text(json.dumps(residuals))

    # Calibration plot
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt

        fig, axs = plt.subplots(1, 2, figsize=(10, 4))
        for ax, axis, sigs, res in (
            (axs[0], "roll", sigmas_roll, resid_roll),
            (axs[1], "pitch", sigmas_pitch, resid_pitch),
        ):
            sigs_np = np.array(sigs)
            res_np = np.array(res)
            # Bin by σ; report mean |residual| per bin.
            edges = np.quantile(sigs_np, np.linspace(0, 1, 11))
            mids, means = [], []
            for lo, hi in zip(edges[:-1], edges[1:]):
                mask = (sigs_np >= lo) & (sigs_np <= hi)
                if mask.sum() < 3:
                    continue
                mids.append(0.5 * (lo + hi))
                means.append(res_np[mask].mean())
            ax.plot(mids, means, "o-", label="empirical mean |resid|")
            ax.plot([0, max(sigs_np)], [0, max(sigs_np)], "k--", label="y=x (perfect)")
            ax.set_xlabel(f"predicted σ_{axis} (rad)")
            ax.set_ylabel(f"empirical |residual| (rad)")
            ax.set_title(f"{axis} calibration  (n={len(sigs)})")
            ax.legend()
            ax.grid(alpha=0.3)
        fig.tight_layout()
        fig.savefig(args.out_dir / "calibration.png", dpi=110)
        plt.close(fig)
    except Exception as e:
        print(f"calibration plot skipped: {e}", file=sys.stderr)

    meta = {
        "architecture": "resnet18-frozen + 2-layer MLP head",
        "loss": "heteroscedastic gaussian NLL (Kendall & Gal 2017), per axis",
        "input_size": INPUT_SIZE,
        "imagenet_mean": list(IMAGENET_MEAN),
        "imagenet_std": list(IMAGENET_STD),
        "epochs": args.epochs,
        "batch_size": args.batch_size,
        "lr": args.lr,
        "n_train": len(train_samples),
        "n_val": len(val_samples),
        "n_panoramas": len({s.pano_path for s in all_samples}),
        "pano_source": "polyhaven.com (CC0)",
        "expected_sigma_floor_rad": float(np.percentile(sigmas_roll, 5)),
        "median_sigma_roll_rad": float(np.median(sigmas_roll)),
        "median_sigma_pitch_rad": float(np.median(sigmas_pitch)),
        "median_abs_residual_roll_rad": float(np.median(resid_roll)),
        "median_abs_residual_pitch_rad": float(np.median(resid_pitch)),
    }
    (args.out_dir / "meta.json").write_text(json.dumps(meta, indent=2))
    print(json.dumps(meta, indent=2))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--pano-dir", type=Path, required=True)
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--per-pano", type=int, default=24)
    ap.add_argument("--epochs", type=int, default=8)
    ap.add_argument("--batch-size", type=int, default=64)
    ap.add_argument("--lr", type=float, default=3e-4)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()
    train(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
