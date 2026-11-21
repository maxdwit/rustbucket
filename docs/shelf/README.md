# Shelf: the Debian image

Until 2026-11-21 the image was Debian (`debian:trixie-slim`, glibc); it was replaced by Alpine (musl), see `../alpine.md`.
`Dockerfile.debian` is the last Debian version, kept for reference only: nothing builds it. To go back, copy it over `/Dockerfile`
(it has the same `server` and `standalone` stages).

- Debian was slightly faster for big downloads (about 640 vs 510 MB/s on loopback; no difference over a real network).
- Its security updates were `apt-get upgrade` in a layer re-run daily through the `PKG_DATE` build arg (Alpine: `apk upgrade`).
- The Debian runtime already had GNU tar and bash; the Alpine one installs them for `make backup` and the entrypoint.
