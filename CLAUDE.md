# CLAUDE.md - instructions for AI agents working in this repository

rustbucket is a Nextcloud-35-compatible file server in Rust (no PHP): desktop/Android/iOS Nextcloud clients and its own
web UI talk to it. **This branch is a public preview / test build.** Read `README.md` first, then this file.

## The one rule about `make`
**Use the Makefile for everything it covers. Do not invent your own `docker run` / `docker build` / `cargo` command lines.**
The targets pin the image names, ports, volumes and flags that the scripts and the docs rely on. Run `make help` first.

| You want to | Run | Notes |
|---|---|---|
| build the image | `make image` | multi-stage `Dockerfile`; the default (last) stage is `standalone` |
| start it | `make run` | `https://localhost:8443/`, container `rustbucket`, volumes `rustbucket-{data,db,certs}` |
| get the first admin password | `make password` | generated on the first start, printed once in the log |
| create a user | `make user U=bob [ADMIN=1]` | password is asked for and goes through stdin; never put it in argv |
| look at it | `make logs` / `status` / `shell` / `cert` | |
| format, lint, test | `make fmt`, `make clippy`, `make test`, `make check` | cargo runs inside a `rust:1` container; **there is no cargo on the host** |
| end-to-end checks | `make smoke` (container over TLS), `make e2e` (real `nextcloudcmd`) | `smoke` needs only docker and curl |
| backup / restore | `make backup`, `make restore FILE=...` | restore only fills **empty** volumes |
| check for personal data | `make audit-pii` | run it before every commit |
| version | `make version`, `make bump-version V=X.Y.Z\|patch\|minor\|major` | version lives in `Cargo.toml`; images are stamped with date + commit by `make image`; bump needs python3 on the host and commits nothing |

Variables you may override: `IMAGE`, `NAME`, `PORT`, `BIND` (`127.0.0.1` by default; `0.0.0.0` serves other devices),
`TLS_NAMES` (names/addresses clients use; they go into the certificate), `DEST`, `RUST_IMAGE`.
Do not add a target that bypasses these; extend the Makefile (and `make help`) instead, and keep it working with only
`make`, `docker`, `curl`, `tar`, `openssl` and `bash` on the host (`make bump-version` additionally needs `python3`).

**Destructive targets are the human's decision:** `make purge` (deletes the container and all its data) needs
`CONFIRM=1`, `make restore` refuses to run over existing data. Do not run them, and do not remove volumes or containers
that you did not create yourself (smoke tests use throw-away names).

## How the standalone container works
One container, two processes, both **unprivileged (uid 10001)**: `nginx` terminates TLS on `:8443` with a
self-signed certificate ("snakeoil") and proxies to `rustbucket serve` on `127.0.0.1:8080`.
- `docker/entrypoint.sh` is PID 1: makes the certificate if `/certs/tls.crt|tls.key` are missing (a supplied one is never
  touched; a generated one is renewed when `RB_TLS_NAMES` changes or 30 days before it ends), migrates the database,
  creates the first administrator when there is no user yet (`RB_ADMIN_USER`, `RB_ADMIN_PASSWORD[_FILE]`, otherwise a
  generated password that is logged once), starts both processes and stops both when either ends.
- `docker/nginx.conf`: streams request bodies (no buffering, no size limit), passes the request target untouched, and
  **overwrites** `X-Forwarded-For` (the server trusts its first entry, so nginx must never append).
- The server trusts `X-Forwarded-*` only from `127.0.0.1/32` (`RUSTBUCKET_TRUSTED_PROXIES`), judged on the **TCP peer**
  (`auth::Peer`), never on the forwarded client address (`auth::ClientIp`, used for rate limits only).
- `docker build --target server` gives the bare server (plain HTTP :8080) that the test scripts use.

## Layout
`src/` server (axum, sqlx/SQLite, askama templates in `templates/`, assets in `static/`), `migrations/` (forward-only
SQL, each with a `.down.sql`), `tests/` (integration tests; `tests/fixtures/nc35` are recorded replies of stock
Nextcloud 35), `scripts/` (end-to-end, differential, backup, fuzz, soak, smoke), `docs/`, `fuzz/` (cargo-fuzz targets).

## Working rules
- Do not push, tag, or add remotes unless the human asks. Commit only when asked; stage files by name.
- **No personal information, ever** - in code, tests, docs, scripts, comments, commit messages or fixtures: no real names,
  email addresses, hostnames, LAN addresses, home paths, tokens or passwords. Use `alice`, `bob`, `carol`, `example.com`,
  `192.0.2.x` (documentation range). `make audit-pii` enforces the mechanical part; the rest is your judgment. Never paste
  a real credential into a file, a log you keep, or a commit.
- Passwords: stdin or a `_FILE`, never a command-line argument or a committed file.
- **Changelog:** every change an operator or user can notice (feature, fix, behaviour change) gets an entry in
  `CHANGELOG.md` in the same commit, and the commit message says the same in a few lines. Describe the problem for a fix, not
  just the code change.
- Tests are part of the change: a bug fix gets a regression test that fails without the fix. SQL is literal (sqlx macros),
  never built from strings. Keep `cargo clippy --all-targets -- -D warnings` clean.
- Compatibility target is Nextcloud **35.x**; behaviour questions are answered by comparing with stock Nextcloud
  (`scripts/diff_*.py`, fixtures), not by guessing. Out of scope: public links, federation, apps, E2E encryption, LDAP.
- Files are stored under their real names (`data/<user>/files/...`); the database is only an index (`rustbucket scan`
  reconciles it). Never edit `data/` or the database by hand while the server runs.
- If a command is refused by a safety check, do not rephrase it to get around the check; ask the human.

## Before you say "done"
`make check` (fmt + clippy + tests), `make smoke` (if you touched the Dockerfile, `docker/`, the server's proxy/TLS
handling or backup), `make audit-pii`. Report what you ran and what you could not run.
