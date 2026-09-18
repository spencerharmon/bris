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

`.github/workflows/corpus-explorer-image.yml` (a Gitea Actions
workflow) builds this image and pushes it **by digest** to the Gitea
OCI registry. Registry host, namespace, and push credentials come
from Actions vars/secrets — never committed. The published digest is
captured as a workflow output; the flux side pins the image by that
digest.

Definition of done: `skopeo inspect docker://$BRIS_EXPLORER_IMAGE`
(the checking context supplies `$BRIS_EXPLORER_IMAGE` as the
`registry/namespace/bris-corpus-explorer@sha256:…` reference) — the
image is pullable by digest, proving it really published.
