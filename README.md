# rustbucket

A Nextcloud-35-compatible file server written in Rust: no PHP, one small container, SQLite. It speaks the protocols of
the official **desktop, Android and iOS clients** (login flow v2, WebDAV, chunked upload, sharing, trash, versions,
previews, search) and has its own **web UI**.

> **Public preview / test build.** It is meant for trying out and for testing with the Nextcloud clients, not yet for
> data you cannot lose. Keep backups (`make backup`), read [docs/security.md](docs/security.md), and expect rough edges.

- Files are stored under their **real names** on disk (`data/<user>/files/...`); the database is only an index.
- Users and groups, user/group shares with the Nextcloud permission bits, trash bin and file versions.
- Verified against stock Nextcloud 35.0.3 with differential tests and with the real sync clients (`nextcloudcmd`, GUI client).
- **Live editor** for text files, several people typing at the same time (see below), also from the Nextcloud mobile apps.
- Web UI with a left menu (own files, shared with me, shared, trash), multi-select, an **image and video viewer** with zoom, and a
  **dark mode** (auto, dark or light).
- Out of scope: public links, federation, apps (calendar, contacts, talk, ...), end-to-end encryption, LDAP.

## Try it - one container, nothing else needed

The image contains the server **and nginx with a self-signed ("snakeoil") certificate**, so TLS works out of the box.
You need `docker` and `make`:

```
git clone <this repository> && cd rustbucket
make run          # builds the image on first use, starts it on https://localhost:8443/
make password     # the generated password of the first administrator, "admin" (shown once)
```

Open <https://localhost:8443/>, accept the certificate warning, log in. In a Nextcloud client use the server address
`https://localhost:8443`. To use it from other devices: `make run BIND=0.0.0.0 TLS_NAMES=localhost,files.example.com,192.0.2.10`
and trust the certificate on the device ([docs/ca-rollout.md](docs/ca-rollout.md)).

`make help` lists everything else: more users (`make user U=bob`), certificate export, logs, backup and restore,
smoke test, and the test targets (no Rust toolchain is needed on the host, cargo runs in a container).

| Read | |
|---|---|
| [docs/install.md](docs/install.md) | the container, its settings, TLS names, your own certificate or reverse proxy, server configuration |
| [docs/ca-rollout.md](docs/ca-rollout.md) | trusting the self-signed (or a private CA) certificate on desktops and phones |
| [docs/admin-cli.md](docs/admin-cli.md) | users, groups, scan, backup, trash, expiry |
| [docs/backup-restore.md](docs/backup-restore.md) | what to back up, how to restore, how it is tested |
| [docs/upgrade.md](docs/upgrade.md) | upgrading and rolling back |
| [docs/operations.md](docs/operations.md) | health, logs, metrics, disk space, troubleshooting |
| [docs/security.md](docs/security.md) | security model and hardening |
| [docs/js-dependencies.md](docs/js-dependencies.md) | the JavaScript in the browser bundle, with versions and licenses; how CVEs are watched |
| [docs/alpine.md](docs/alpine.md) | why the image is Alpine, sizes, differences; the old Debian Dockerfile is in `docs/shelf/` |
| [CHANGELOG.md](CHANGELOG.md) | what changed, including every fix |
| [CLAUDE.md](CLAUDE.md) | instructions for AI agents (how to use the Makefile, rules for changes) |

## The web UI: live editor, viewer, dark mode

- **Live editor.** A click or tap on a `.md`, `.txt`, `.sh`, `.bash`, `.py`, `.json`, `.yml`, `.yaml` or `.conf` file opens it in an
  Etherpad-style editor: everybody who may **read** the file sees changes live, only users with **change** permission can type
  (the server enforces that, not the browser). The text is saved automatically and with *Save & close* like a normal upload
  (version history and sync clients see it). *Undo* and *Redo* roll back everybody's changes and must be held for one second.
  The Nextcloud Android app opens text files in the same editor (direct editing). It is built on Yjs (`yrs` on the server,
  CodeMirror 6 in the browser); switch it off with `RUSTBUCKET_EDITOR=false`. Behind your own reverse proxy it needs WebSocket
  support ([docs/install.md](docs/install.md)).
- **Viewer.** A click on a PNG, JPEG, GIF, WebP, BMP, WebM or MP4 file shows it full screen; zoom with the mouse wheel, pinch,
  `+`/`-` or double click, move it when zoomed, `X` (top right) or Esc closes, arrows go to the next item. *Download* is in the row menu.
- **Dark mode.** The button in the top bar cycles auto (follows the system), dark and light.
- The few JavaScript files are listed with versions and licenses in [docs/js-dependencies.md](docs/js-dependencies.md).

## Tests

Everything runs in containers; the first column is what you type.

| Command | What it checks | Needs on the host |
|---|---|---|
| `make check` | rustfmt, clippy (warnings are errors) and the unit and integration tests | docker |
| `make smoke` | builds the image, starts it, probes it over TLS: 30 MB streamed upload, chunked upload with a gap, live editor WebSocket through nginx, restart, backup, purge, restore, login throttle | docker, curl |
| `make audit-deps` | known vulnerabilities in the Rust dependencies (`cargo audit`, `cargo deny`) | docker |
| `make audit-js` | known vulnerabilities in the editor's JavaScript (`npm audit`, registry signatures) | docker |
| `make editor-check` | the committed `static/editor.js` is exactly what the lockfile builds | docker |
| `make scan-image` | CVEs and secrets in the built image (Trivy) | docker |
| `make audit-pii` | no private addresses, home paths or real-looking e-mail addresses in the tree | - |
| `scripts/e2e.sh` | a real Nextcloud sync client (`nextcloudcmd`) against a throw-away server | `nextcloudcmd` |
| `scripts/e2e_web.sh` | the web UI in Chromium: upload, share, viewer, dark mode, two browsers in the live editor | `chromium`, `chromedriver` |
| `scripts/diff_*.py` | the same requests against stock Nextcloud and rustbucket, differences listed | a Nextcloud container |
| `scripts/backup_restore_test.sh` | backup and restore round trip | docker |
| `scripts/fuzz.sh` | fuzzing of parsers (nightly toolchain, `cargo-fuzz`) | rust nightly |
| `scripts/soak.sh` | 10 parallel clients for an hour | docker |

License: WTFPL (see `LICENSE`).
