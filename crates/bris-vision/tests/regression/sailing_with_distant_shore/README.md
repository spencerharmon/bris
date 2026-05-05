# Regression case: sailing scene with distant shore between sea and sky

A sailing-vessel POV looking forward, with the Sun visible high in
frame, sun glare reflected on the water on the right, and a **distant
shoreline visible on the right side of the frame between sea and
sky**. This is the case the obstruction-aware horizon detector
(catalog item 3) is specifically designed to handle.

## Why this case matters

The simple sky→sea transition algorithm would skip every column
where the distant shore appears because the first non-sky pixel is
land, not sea. The obstruction-aware variant looks past the shore
for sea below it and accepts the column with the obstruction's top
row as the horizon candidate, tagged as a lower-confidence
SkyToObstructionToSea source.

On this scene with the obstruction-aware detector:
- 162 columns: clean sky → sea (no shore in column).
- **168 columns: sky → thin shore → sea** (this is the obstruction-
  aware code's contribution — without it, these would be skipped).
- 94 columns: sky → boat/structure (occluded; rejected).
- 88 columns: no sky visible (boat reaches top of frame).

Total accepted: 330 columns out of 512, vs. 162 with strict.

## Frames

- `frame.png` — first frame of the new sailing-test sweep (640×360).

## Conditions

- **Source.** User-provided sailing footage.
- **Day / night / twilight.** Day, late afternoon based on lighting.
- **UTC and observer position.** Unknown. For replay use placeholder
  values; intercepts will be meaningless but per-column counts are
  the load-bearing assertion for this case.
