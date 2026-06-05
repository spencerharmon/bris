# ML-gravity model training (Phase 7.7a results)

Status: live as of this commit.

Operator handoff 2026-06-05: B1 (license) and B2 (vendoring)
both cleared. GPU + podman + nvidia-container-toolkit
verified. See commit `0b2c306` for the operator confirmations.

## Architecture

| component         | choice                                                                                                                                                                                                                                                                                       |
|-------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| backbone          | torchvision `resnet18` with ImageNet-1k pretrained weights, **frozen**                                                                                                                                                                                                                       |
| head              | 2-layer MLP (512 → 256 → 4), GELU activation                                                                                                                                                                                                                                                 |
| output            | (roll, pitch, log_var_roll, log_var_pitch)                                                                                                                                                                                                                                                   |
| loss              | per-axis heteroscedastic gaussian NLL per Kendall & Gal (NeurIPS 2017): `L = 0.5 · (exp(-s)·(μ-y)² + s)`. Roll residual uses shortest angular distance on (-π, π].                                                                                                                            |
| input             | NCHW float32 (1, 3, 256, 256), ImageNet-normalised                                                                                                                                                                                                                                           |
| training set      | Polyhaven CC0 panoramas (60 panoramas × 32 random tilts each = 1920 samples), 10% held-out for validation                                                                                                                                                                                    |
| epochs            | 10                                                                                                                                                                                                                                                                                           |
| optimiser         | AdamW (lr 3e-4, weight decay 1e-4), cosine LR schedule                                                                                                                                                                                                                                       |
| compute           | NVIDIA RTX 3080 (CUDA 12.4, podman container), ~14 s/epoch, full run ~2.5 min                                                                                                                                                                                                                |

## Tradeoff: backbone choice (operator may revisit)

`docs/design/ml_gravity.md` names GeoCalib as the
preferred model. GeoCalib's published checkpoints require
building custom CUDA extensions that don't fit a single-pass
CI-reproducible container; using them would have required
either a multi-day vendor-the-extensions detour or shipping
weights that don't reproduce from `scripts/ml-gravity/`.

The design doc's actual contract — *Layer-2 heteroscedastic
σ on a 4-scalar output* — is architecture-agnostic. We ship
a ResNet18-frozen + heteroscedastic head that satisfies the
contract from a single Dockerfile + Python script. The
provider's `model_id` (12-char BLAKE3 truncation, surfaced
in `HorizonProvenance::MlGravity`) distinguishes this
model from any future GeoCalib export; consumers can
discriminate without code changes.

If marine fine-tune (Phase 7.7d) wants GeoCalib's stronger
backbone, the swap is a Python-side change to the
`HeteroscedasticHead` constructor — the rest of the
pipeline (input shape, output shape, σ propagation in the
Rust provider) is unchanged.

## License

- **Polyhaven panoramas**: CC0 (`https://polyhaven.com/license`).
  Public-domain; redistributable inside a GPL-3.0-or-later
  binary or release artifact.
- **Torchvision ResNet18 ImageNet-1k weights**: BSD-3.
  Compatible with GPL-3.0-or-later.
- **Trained ML-gravity ONNX file**: derivative work of the
  above; redistributable under GPL-3.0-or-later (the
  workspace license). Operator confirmed at kickoff
  2026-06-05.

## Reproduction

```sh
# Build the container (CUDA 12.4 + cuDNN devel + pinned deps):
podman build -t bris-mlgrav -f scripts/ml-gravity/Containerfile .

# Fetch CC0 panoramas to /tmp/polyhaven:
python3 scripts/ml-gravity/fetch_polyhaven_panos.py \
    --out /tmp/polyhaven --count 60

# Train (GPU required; container handles CUDA stack):
podman run --rm --device nvidia.com/gpu=all --shm-size=4g \
    -v "$PWD":/work -w /work \
    -v /tmp/polyhaven:/data/polyhaven:ro \
    -v /tmp/mlgrav-train:/out \
    bris-mlgrav \
    python3 scripts/ml-gravity/train_heteroscedastic.py \
        --pano-dir /data/polyhaven --out-dir /out \
        --per-pano 32 --epochs 10

# Export to ONNX:
podman run --rm \
    -v "$PWD":/work -w /work \
    -v /tmp/mlgrav-train:/in \
    bris-mlgrav \
    python3 scripts/ml-gravity/export_onnx.py \
        --weights /in/weights.pt \
        --out /work/data/ml-gravity/geocalib-heteroscedastic-v1.onnx
```

Outputs land at `data/ml-gravity/` along with
`SHA256SUMS` for the fetch-at-build path.

## Validation results

See `data/ml-gravity/training/meta.json` for the live
numbers from the most-recent training run vendored along
with this commit. Calibration plot at
`docs/design/ml_gravity_calibration.png` (vendored PNG).

## Honesty: known σ-floor regimes

The training distribution is indoor-heavy (Polyhaven's
catalog leans into indoor architecture HDRIs). On the
held-out validation set the model produces honest per-axis
σ that bins monotonically against actual residual.

What the model has NOT been validated on, and where σ is
expected to be **under**-estimated (over-confident):

- Open-water / horizon-visible marine scenes (out of
  training distribution).
- Pure-sky / pure-water frames with no orientation cues
  (the model can't see "down" if no scene structure exists;
  its σ will be honest but the prediction is essentially
  the dataset prior — gravity ≈ image-down).
- Heavily-tilted captures past ±45° pitch (training tilts
  are uniform in [-π/4, π/4]; outside that band the model
  extrapolates).

These limitations are documented in `docs/design/ml_gravity.md`
§"Marine vs land-based" and addressed by Phase 7.7d
(deferred until the trainer APK Phase 7.7e produces a
marine corpus).

## Roll-axis honesty

Roll wraps over (-π, π]. The loss uses
`atan2(sin Δ, cos Δ)` shortest-angular-distance to keep
the residual well-defined across the wrap point. Even so,
the σ_roll on a pure-sky frame (no roll cue available)
converges to large values reflecting the model's lack of
confidence; in such cases the synthesized horizon line has
a wide tilt prior and Stage C fusion correctly down-weights
it against any geometric provider that fires.
