# rustbucket

A Nextcloud-35-compatible file server written in Rust: no PHP, one small container, SQLite. It speaks the protocols of
the official **desktop, Android and iOS clients** (login flow v2, WebDAV, chunked upload, sharing, trash, versions,
previews, search) and has its own **web UI**.

> **Public preview / test build.** It is meant for trying out and for testing with the Nextcloud clients, not yet for
> data you cannot lose. Keep backups (`make backup`), read [docs/security.md](docs/security.md), and expect rough edges.

- Files are stored under their **real names** on disk (`data/<user>/files/...`); the database is only an index.
- Users and groups, user/group shares with the Nextcloud permission bits, trash bin and file versions.
- Verified against stock Nextcloud 35.0.3 with differential tests and with the real sync clients (`nextcloudcmd`, GUI client).
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
| [docs/alpine.md](docs/alpine.md) | the container on Alpine instead of Debian: it works, about 30 % of the size (experiment) |
| [CHANGELOG.md](CHANGELOG.md) | what changed, including every fix |
| [CLAUDE.md](CLAUDE.md) | instructions for AI agents (how to use the Makefile, rules for changes) |

## Tests

`make check` runs fmt, clippy and the ~130 unit and integration tests; `make smoke` exercises the container over TLS
(30 MB streamed upload, chunked upload with a gap, restart, backup, purge, restore). Further scripts in `scripts/` need extra tools on the host:
`e2e.sh` (real `nextcloudcmd`), `e2e_web.sh` (Chromium), `diff_*.py` (against a stock Nextcloud container),
`backup_restore_test.sh`, `fuzz.sh` (nightly toolchain, cargo-fuzz), `soak.sh` (10 parallel clients for an hour).

License: WTFPL (see `LICENSE`).
