# Corpus-explorer static-server image.
#
# Packages the zero-dependency static corpus-explorer SPA
# (tools/corpus-explorer/) behind nginx and serves a merged web
# root over HTTP:
#
#   /                          → 302 to the explorer entrypoint
#   /tools/corpus-explorer/    → the baked SPA assets (this image)
#   everything else            → the PVC-mounted corpus data-root
#                                (index.json, sessions/<uuid>/…,
#                                 -render.png overlays)
#
# The SPA's explorer.js uses CORPUS_ROOT = "../../", so when the
# page is served at /tools/corpus-explorer/index.html the browser
# resolves index.json + session reports at the web root — which is
# exactly where the corpus data-root is mounted.
#
# INFRA-AGNOSTIC: this image hard-codes NO host, domain, TLS, or
# registry identifier. The corpus data-root PVC is mounted at the
# fixed web-root path /srv/corpus (the deploy contract nginx.conf
# and README.md pin); the flux side owns the PVC, the mount,
# traefik/TLS, and DNS. See tools/corpus-explorer/deploy/README.md.

FROM docker.io/nginxinc/nginx-unprivileged:1.27-alpine

# The fixed web root nginx serves; the corpus data-root PVC is
# mounted HERE by the flux Deployment. The explorer SPA is baked
# OUTSIDE it (/opt/corpus-explorer) so the mount cannot shadow it.
ARG CORPUS_WEB_ROOT=/srv/corpus

USER root

# Baked SPA assets. Copied into a location OUTSIDE the web root so
# a PVC mounted at $CORPUS_WEB_ROOT never shadows them; nginx maps
# /tools/corpus-explorer/ onto this path via `alias`.
COPY tools/corpus-explorer/index.html \
     tools/corpus-explorer/explorer.css \
     tools/corpus-explorer/explorer.js \
     tools/corpus-explorer/README.md \
     /opt/corpus-explorer/
COPY tools/corpus-explorer/data/ /opt/corpus-explorer/data/

COPY tools/corpus-explorer/deploy/nginx.conf /etc/nginx/nginx.conf

# The web root must exist even before the PVC is mounted so nginx
# starts cleanly; the mount overlays it at runtime.
RUN mkdir -p "$CORPUS_WEB_ROOT" \
    && chown -R nginx:nginx /opt/corpus-explorer "$CORPUS_WEB_ROOT"
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
