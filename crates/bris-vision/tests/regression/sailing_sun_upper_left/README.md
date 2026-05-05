# Regression case: sailing scene with sun in upper-left

A sailing-vessel POV with the sun visible in the upper-left of the
frame, the boat's mainsail occupying the middle-right, and a small
visible sea-sky boundary on the right. The horizon is partially
occluded by the boat structure and rigging.

This case was the first real-world test that exposed two algorithm
failures and motivated adding ML-based segmentation:

1. **Gradient horizon detector** picks the deck-to-sea boundary on
   the left rather than the actual sea-sky horizon on the right
   (deck has stronger horizontal gradient than the small visible
   horizon). Reported altitude was ~13.5° using placeholder
   intrinsics; actual horizon is much higher in the frame.

2. **Sky-region horizon detector** finds the top of the mainsail
   (sky → sail-edge transition) rather than the actual horizon.
   Reported altitude was ~3° — different wrong answer.

3. **Segmentation horizon detector** correctly identifies sky/sea/
   ship pixels. With the source image re-loaded as RGB (not Bris's
   grayscale-replicated three channels), 172 of 512 columns produce
   clean sky→sea transitions. Reported altitude is ~14° with σ ~3'.
   The remaining ~14° vs. visually-estimated ~50° discrepancy is
   the placeholder intrinsics (fy=1000 vs. real GoPro fy ~400 for
   360-px-tall frames).

## Frames

- `frame.png` — first frame of the sweep (frame 0001 in the source).
- `frame_5s_later.png` — same scene, 5 seconds later (frame 0300).
  Demonstrates that the boat's pitch/roll motion is small over
  this interval; the two frames are visually almost identical.

## Conditions

- **Source.** A YouTube sailing video; user-provided test material.
- **Day / night / twilight.** Day, clear sky.
- **UTC.** Unknown (the source video has no GPS or timestamp
  metadata). For replay, use `--capture-utc 2024-03-15T15:00:00Z`
  as a placeholder; intercepts will be meaningless but pipeline
  behavior is unchanged.
- **Observer position.** Unknown.
