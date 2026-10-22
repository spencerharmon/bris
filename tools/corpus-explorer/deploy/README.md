# Corpus-explorer static-server image — deploy contract

This directory packages the zero-dependency corpus-explorer SPA
(`tools/corpus-explorer/`) into an OCI image that serves the stored
corpus over HTTP. The image is **infra-agnostic**: it contains no
host, domain, TLS, DNS, or registry identifier. The flux side owns
the PVC, the mount, traefik/TLS, and DNS.

## What the image does

`nginx` (the unprivileged `nginxinc/nginx-unprivileged` base) serves
a merged web root on **plain HTTP port 8080**:

| Path | Served from | Notes |
|------|-------------|-------|
| `/` | — | `302` redirect to `/tools/corpus-explorer/index.html` |
| `/healthz` | — | `200 ok` liveness/readiness probe, no data-root dependency |
| `/tools/corpus-explorer/…` | baked image assets at `/opt/corpus-explorer/` | the SPA (`index.html`, `explorer.js`, `explorer.css`, `data/`) |
| everything else | the corpus data-root at `/srv/corpus` | `index.json`, `sessions/<uuid>/…`, `-render.png` overlays |

The SPA's `explorer.js` uses `CORPUS_ROOT = "../../"`. Served at
`/tools/corpus-explorer/index.html`, that resolves the corpus
`index.json` and every session `report_path` / `render_path` at the
web root — which is exactly where the corpus data-root is mounted.
The SPA assets are baked **outside** the web root (`/opt/corpus-explorer`,
mapped in via nginx `alias`) so a PVC mounted at `/srv/corpus` cannot
shadow them.

## The flux-side deploy contract

The flux Deployment consuming this image must:

1. **Mount the corpus data-root PVC at `/srv/corpus`** (read-only is
   fine; the explorer never writes). This is the PVC the collector
   writes its durable store to — the tree containing `index.json` and
   `sessions/<uuid>/{bris-replay-report.json, captures/…/frames/…-render.png}`.
2. **Expose container port 8080** behind a Service.
3. **Front it with traefik** for host/TLS/DNS. All of those live
   ONLY on the flux side — never in this image.

The container runs as the unprivileged `nginx` user (uid 101 in the
base image) and needs no extra capabilities.

## Building locally

```sh
docker build -f tools/corpus-explorer/deploy/Dockerfile \
  -t bris-corpus-explorer:dev .
```

(The build context is the repo root so the SPA assets under
`tools/corpus-explorer/` are copyable.)

`tools/corpus-explorer/deploy/verify_image.sh` builds the image and
asserts the merged-root routing (redirect, healthz, baked SPA, and a
data-root file mounted at `/srv/corpus`) actually works in a running
container. It is the honest regression test for this image.

## Publishing

`.github/workflows/corpus-explorer-image.yml` runs in GitHub Actions
and publishes `ghcr.io/<lowercase GitHub owner>/bris-corpus-explorer`
with the full publishing commit SHA as its tag. It authenticates with
the automatic `GITHUB_TOKEN` and grants `packages: write` plus
`contents: read`; no registry vars or separately provisioned push
secrets are required. The published digest is captured as a workflow
output; the flux side pins the image by that digest.

GHCR package visibility is a separate requirement: new packages default
to private. An authorized package owner must select public visibility
in package settings for anonymous consumers. The OCI source label links
the package to the repository; it does **not** make the package public.
The workflow's anonymous inspection deliberately fails for private images.

For post-merge verification, retain the publishing run's full commit SHA,
not the current `origin/main` tip: path filters skip unrelated pushes.
`python3 scripts/publishing_revision.py <implementation-commit-sha>`
queries the public GitHub Actions API for `collector-image.yml` push runs
on main, then selects the earliest run SHA on the fetched, non-shallow
`origin/main` first-parent history that contains the implementation. This
handles a single fast-forward push containing multiple implementation commits;
the intermediate commits are not image tags. The repository is derived from
the GitHub `origin` URL; no API credential or extra Python dependency is needed.
Missing runs, unavailable history, API errors and rate limits fail closed.

This resolver is for coordinated changes (such as the GHCR migration) whose
landing push triggers all three image workflows. It does not assert build
success: failed and pending runs remain eligible so retries keep checking the
same SHA, even after unrelated pushes. Inspect collector, explorer and CLI
tags at that SHA with `skopeo inspect --no-creds`. Retain the resolved SHA in
the deployment record before GitHub run history expires or is deleted; run
metadata is not a permanent publication ledger.

Definition of done: `skopeo inspect --no-creds docker://$BRIS_EXPLORER_IMAGE`
(the checking context supplies `$BRIS_EXPLORER_IMAGE` as the
`registry/namespace/bris-corpus-explorer@sha256:…` reference) — the
image metadata is accessible anonymously by digest, proving it really
published with public visibility rather than relying on a cached login.
