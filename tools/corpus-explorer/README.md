# Bris corpus explorer

A zero-dependency, vanilla HTML/JS single-page app for browsing
`bris replay --render-frames` artefacts.

## Prerequisites

1. Run replay with rendering and report generation enabled:

   ```sh
   # over the entire corpus
   bris replay --corpus path/to/bris-corpus --all-sessions --render-frames

   # or one session at a time
   bris replay --corpus path/to/bris-corpus --session <UUID> --render-frames
   ```

   This writes `<corpus>/sessions/<UUID>/bris-replay-report.json`
   for each session, plus (for `--all-sessions`) a corpus-root
   `<corpus>/index.json` cataloguing them.

2. Confirm the tree on disk:

   ```
   bris-corpus/
     index.json
     sessions/<UUID>/
       bris-replay-report.json
       session.json
       captures/<id>/
         bundle.json
         frames/
           00000000.pgm
           00000000.json
           00000000-render.png    ← the annotated overlays
   ```

## Running the explorer

The explorer is plain HTML + ES-module JavaScript. It needs to
be served (browsers refuse `fetch()` from `file://`). The
simplest server is Python's built-in:

```sh
cd path/to/bris-corpus    # the corpus root, NOT the tools dir
# (the tools directory must be reachable underneath; if your
#  checkout is alongside the corpus, symlink it in:
#  ln -s ../bris/tools .  )
python3 -m http.server 8765
```

Then point a browser at:

```
http://localhost:8765/tools/corpus-explorer/index.html
```

The page fetches `index.json` from the corpus root and lazily
fetches each session's report when you click on it. PGMs are
never loaded by the explorer — only the pre-rendered
`-render.png` annotations.

### Alternative layout

If your `tools/` directory lives outside the corpus (the common
case during development), the simplest setup is to serve from
the repo root and pass the corpus root as a sibling path:

```sh
cd ~/bris
ln -sf bris-corpus tools/corpus-explorer/_corpus
python3 -m http.server 8765
# then edit explorer.js's CORPUS_ROOT to "./_corpus/"
```

Or just symlink the tools directory into the corpus root, as
shown above — that's what `CORPUS_ROOT = "../../"` in
`explorer.js` is sized for.

## Interaction

- Left sidebar: list of sessions (click to load).
- Main view: one block per capture with frame thumbnails.
  - Thumbnail badge `✓` indicates Stage E emitted a sight.
  - Hover for the per-frame outcome summary.
  - Click to open the full-size annotated PNG in a lightbox.
- The Stage E rejection histogram per capture surfaces the
  most common reasons sights got dropped (the standout being
  `BelowHorizon` — fix-frame geometry doesn't match the
  computed altitude).

## When things go wrong

- *"could not load index.json"* — replay wasn't run with
  `--all-sessions --render-frames`, or you're serving from the
  wrong directory.
- Thumbnails fail to load — re-run replay with
  `--render-frames`; older session reports may reference
  render PNGs that haven't been generated yet.
- Blank thumbnails — the PGM was too dim (auto-level uses
  1st/99th percentile, very flat scenes can produce nearly-
  black overlays). Inspect with an image viewer to confirm.

The explorer is intentionally simple — no build step, no npm,
no framework. To extend it, edit `explorer.js` / `explorer.css`
directly and reload the page.
