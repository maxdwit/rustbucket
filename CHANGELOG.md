# Changelog

All notable changes to this public preview. Newest first. The version is the one in `Cargo.toml`
(`make bump-version`); every build is also stamped with its date and commit (Admin > About, `rustbucket version`).

## Unreleased (2026-11-21, after the first public commit)

### Added
- **Left menu in the web client**: *My own files* (only what you own), *Shared with me* (incoming shares with owner,
  permission and date), *Shared* (what you share with others, with recipients and permissions) and *Trash*. The views
  are `?view=shared-with-me|shared`; no `view` means your own files, so old links keep working. Sync clients and
  WebDAV are unchanged: they still see incoming shares at the root, only the web view separates them. At phone width the
  menu becomes a row.
- **Share as an icon, a "..." menu for the rest, and multi-select**: every row has a Share icon button first
  (highlighted when the item is shared) and a "..." menu with Rename, Move, Versions and Delete (Delete keeps its
  "Sure?" click). Every row has a checkbox and the header a select-all; a bar above the list offers Move (target folder) and
  Delete (confirmation click) for the selection. An action is offered only when every selected item allows it; failures
  are listed by name. On a phone a row becomes two lines with the icons beside the name.
- **Version and build number**: version from `Cargo.toml` (`make bump-version V=X.Y.Z|patch|minor|major`, which also
  updates `Cargo.lock` and `fuzz/Cargo.lock`); the image is stamped with the UTC build date and the git commit
  (`make image`); the build id is a short hash of both plus the version. Shown to administrators in a new **Admin menu**
  (Users, About, and the version line), on a new **About page** (`/settings/about`), by `rustbucket version`, in the
  `rustbucket_build_info` metric and as OCI image labels.
- `make smoke` tests a chunked upload (WebDAV chunking v2) through nginx with curl: session, out-of-order chunks, a
  refused assembly with a missing chunk, the final MOVE, byte comparison and cleanup. It also checks that the build
  number is stamped into the image.
- `make user` (and the other make targets) are described for AI agents in `CLAUDE.md`.

### Fixed
- **Stale scripts and styles after an update**: static files are cached for an hour and were keyed only by the Cargo
  version, so a new image without a version bump could serve a new page with old JavaScript or CSS. The asset links now
  carry the build id, which changes with every build.
- **`make user ADMIN=0` created administrators**: `$(if $(ADMIN),--admin)` treated any non-empty value as true. Now
  `0`, `no`, `false` and empty mean a normal user; `ADMIN=1` (or any other value) makes an administrator.
- **Credential files in the working tree**: `.env_*` and `*.env` are git-ignored (`.env.example` stays tracked).

### Changed
- Tests: a web test for the three views (redirects, escaping, unsharing), one for the icon/menu/checkbox markup and the
  permission flags, one for the Admin menu, About page and asset links; Chromium tests for switching views, multi-select
  (move, delete, trash) and the phone layout; the metrics test checks the full `rustbucket_build_info` labels.

## First public commit (2026-11-21)

Single-container standalone image (the server plus nginx with a self-signed certificate), driven by the Makefile.
Fixed while preparing it:
- **Trusted-proxy check used the wrong address**: behind a trusted proxy and without a pinned `RUSTBUCKET_BASE_URL`, the
  login flow handed out `http://` URLs and cookies lacked the `Secure` flag, because the handlers passed the forwarded
  *client* address (`X-Forwarded-For`) to the trust check instead of the TCP peer. New extractor `auth::Peer` for these
  decisions; `ClientIp` stays for rate limits and logs. Regression test:
  `forwarded_proto_is_honoured_from_a_trusted_peer_only`.
- `make restore` only fills empty volumes and refuses to run over existing data; `make purge` needs `CONFIRM=1`.
- The login throttle cannot be dodged with a forged `X-Forwarded-For` (nginx overwrites the header; tested in
  `make smoke`).
