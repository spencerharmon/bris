# bris-collector — reference deployment contract

**Infrastructure-agnostic reference only.** These files document
the *shape* of the collector's configuration and the env / mount /
port contract the deployment layer (flux) must satisfy. Every
concrete value here is an RFC2606 (`example.com`) or RFC5737
(`192.0.2.0/24`) placeholder. Real hostnames, image tags, the
admin token, the storage class, the PVC size, and the retention
window live **only** in flux — never in this repo.

## Files

- `collector.reference.toml` — the full config surface with every
  key documented and defaulted. Mount a filled-in copy via a
  `ConfigMap` and point `BRIS_COLLECTOR_CONFIG` at it.
- `reference-deployment.yaml` — a reference `Deployment` + `Service`
  fragment showing the env vars, the data-root mount, and the
  container port to copy into the real overlay.

## Configuration precedence

`Config::load` resolves settings last-wins:

1. built-in defaults,
2. the TOML config file (from the `BRIS_COLLECTOR_CONFIG` path, if
   set),
3. `BRIS_COLLECTOR_*` environment variables.

So bake a base config file into a `ConfigMap` and override any
single value — most importantly the secret token — from the
environment (a mounted `Secret`) without editing the file.

## Contract the flux consumer fills in

| Setting | Env var | File key | Default | k8s meaning |
|---|---|---|---|---|
| Data root | `BRIS_COLLECTOR_DATA_ROOT` | `data_root` | *(required)* | mount path of the submissions PVC |
| Bind addr | `BRIS_COLLECTOR_BIND` | `bind` | `0.0.0.0:8443` | port → `containerPort` the Service targets |
| Admin token | `BRIS_COLLECTOR_BEARER_TOKEN` | `bearer_token` | *(required at startup)* | from a `Secret`; binary refuses to start without it |
| Max body | `BRIS_COLLECTOR_MAX_SUBMISSION_BYTES` | `max_submission_bytes` | 512 MiB | request-body ceiling |
| Retention | `BRIS_COLLECTOR_RETENTION_DAYS` | `retention_days` | 30 | default `retention-sweep` window (never auto-run) |
| Config file | `BRIS_COLLECTOR_CONFIG` | *(n/a)* | *(env-only if unset)* | path to the mounted config file |

Fixed contract values the manifest and config must agree on:

- **Container port `8443`** — must equal the port half of `bind`.
- **Data-root mount `/var/lib/bris-collector`** — the volume
  `mountPath` must equal `data_root`.
- **Config mount `/etc/bris-collector/collector.toml`** — the path
  `BRIS_COLLECTOR_CONFIG` names.
