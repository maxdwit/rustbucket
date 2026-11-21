# Running the container on Alpine instead of Debian (experiment)

**Result: it works.** `docker/Dockerfile.alpine` builds the same two images (bare server and the nginx + server
"standalone" image) on Alpine Linux (musl libc). They pass everything the Debian images pass, at about **30 % of the size**.
The Debian image stays the default; this page records what was tried and what to expect if you switch.

Build and test it (nothing else changes):

```
make image DOCKERFILE=docker/Dockerfile.alpine IMAGE=rustbucket:alpine
make smoke DOCKERFILE=docker/Dockerfile.alpine IMAGE=rustbucket:alpine-smoke
make scan-image SCAN_IMAGE=rustbucket:alpine
```

## Size (2026-11-21, same commit, security updates applied in both)

| Image | Debian trixie-slim | Alpine 3.24 | Difference |
|---|---|---|---|
| standalone (nginx + server), on disk | 205.5 MB | **60.4 MB** | -145 MB (-71 %) |
| standalone, compressed (`docker save \| gzip`) | 53.2 MB | **17.3 MB** | -36 MB (-67 %) |
| bare server stage, on disk | 174.1 MB | **44.5 MB** | -130 MB (-74 %) |
| bare server stage, compressed | 44.8 MB | **12.5 MB** | -32 MB (-72 %) |
| the `rustbucket` binary | 16.1 MB (glibc) | 15.6 MB (musl) | about the same |

The saving is the operating system layer: `debian:trixie-slim` is 119 MB before we add anything, `alpine` about 8 MB.

## What was verified on Alpine

- `make smoke`: all checks pass - TLS with the generated certificate, 30 MB streamed upload, chunked upload with a gap,
  restart keeps certificate and data, `make backup` / `make purge` / `make restore`, the login throttle with forged
  `X-Forwarded-For`, the build stamp, runs unprivileged.
- `scripts/e2e.sh` with the real `nextcloudcmd` against the Alpine **server** image (100 files, chunked upload, sharing,
  trash, versions, group share): passes.
- `make scan-image` (Trivy): **0** HIGH/CRITICAL findings on Alpine 3.24.4 (Debian 13.8: also 0 since the security-update
  layer was added).
- The whole dependency tree, including SQLite compiled from C, builds for musl with `rust:alpine` without changes.

Not tested on Alpine: the Chromium tests (they do not use the image), a long soak run, arm64.

## What had to be different

- **Build stage**: `rust:1-alpine` plus `musl-dev gcc` (SQLite is compiled from C). No source change was needed.
- **GNU tar and bash in the runtime image**: busybox `tar` has no `--transform`, which `make backup` uses, and the entrypoint
  uses bash arrays and `wait -n`. `apk add nginx openssl curl bash tar`.
- User creation with `adduser -S`, default nginx site removed from `/etc/nginx/http.d/`.
- Security updates: `apk upgrade --no-cache` instead of `apt-get upgrade` (the same daily date argument re-runs the layer).

## Performance (same host, 200 MB through nginx + TLS, two runs each, alternating)

| | Debian | Alpine |
|---|---|---|
| start to answering `/health` | 0.3 s | 0.3 s |
| login (argon2) median, `PROPFIND` with Basic auth | 11 ms | 13 ms |
| 200 MB `PUT` | 646 MB/s | 660 MB/s |
| 200 MB `GET` | 638 MB/s | 509 MB/s (about 20 % lower in both runs) |
| server process RSS after the transfers | 44 MB | **10 MB** |

Login and upload are within noise. Download was consistently somewhat slower on musl. Memory is much lower: glibc's malloc
keeps arenas around after the transfer, musl gives memory back. For a family server none of this matters; for many
parallel clients, measure again.

## Caveats if you adopt it

- **Two Dockerfiles to keep in sync** (and CI would have to run the smoke test for both), or drop the Debian one.
- `alpine:3` and `rust:1-alpine` are tags here; the Debian Dockerfile pins digests. Pin them (and let Renovate update them) before
  using this for real.
- musl differs from glibc in details (DNS resolver, locale, malloc, default thread stack size). Nothing showed up in the tests
  above, but those are the places to look if something odd happens.
- Alpine's `nginx` package is built differently from Debian's (paths, modules); our `docker/nginx.conf` uses only core modules
  and worked unchanged.
- Alpine releases are supported for about two years; plan an upgrade of the `3.x` base.

## Recommendation

Worth adopting if image size or memory matters (small VPS, slow upload of images): the smoke test, the real-client suite and
the scan all pass. Keep Debian as the default until the digests are pinned and CI covers both, then decide whether to keep
one of them.
