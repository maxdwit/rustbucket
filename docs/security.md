# Security model

- **Authentication:** passwords are stored as argon2 hashes. Clients use **app passwords** (72 random characters, only their
  SHA-256 is stored, revocable per device in Settings; revoking signs that device out at once). Basic auth is accepted on
  DAV/OCS; the browser uses a session cookie (HttpOnly, SameSite=Lax, `Secure` behind https).
- **CSRF:** the web UI sends `X-CSRF-Token`, derived from the session id; every non-read request with a session cookie needs
  it. A request with a (wrong) `Authorization` header never falls back to the cookie. Credential endpoints
  (`core/getapppassword`, `core/apppassword`) refuse cookies entirely. Forms carry the token as a hidden field.
- **Browser hardening:** pages have no inline script or style (CSP `script-src 'self'`), `X-Frame-Options`, `nosniff`,
  `Referrer-Policy: no-referrer`; HSTS when the public URL is https. Everything under `/remote.php/` (user uploads!) is
  served with `Content-Disposition: attachment` and `Content-Security-Policy: default-src 'none'; sandbox`.
  File and folder names are HTML-escaped (tested with hostile names) and URLs are percent-encoded.
- **Brute force:** 5 failures per IP / 10 minutes (then 429), constant-time-ish verification for unknown users, at most 4 concurrent
  password hashes. There is deliberately **no per-account lockout**: it would let anybody lock the family out.
- **Paths:** every path is percent-decoded once, NFC-normalised and checked (no `..`, no NUL, no symlink following); user
  names are valid folder names; shares and previews resolve through the same permission checks as DAV.
- **Sharing:** read/update/create/delete bits enforced on every operation, including chunked uploads, LOCK and cross-share
  moves; deleting a mount only unshares. Resharing, public links and federation do not exist (not advertised).
- **Data safety:** all writes are atomic renames; an upload never replaces a file with a partial one; the previous content of an
  overwritten or deleted file is kept as a version / in the trash (retention configurable).
- **Robustness:** parsers are fuzzed (`scripts/fuzz.sh`, nightly) and hit with random garbage in every test run (no panics, no 5xx).
- **TLS:** the standalone container terminates TLS in nginx with a **self-signed certificate** generated at first start
  (private key mode 600, never in the image; both processes run as an unprivileged user; the server itself listens on
  loopback only). A self-signed certificate protects against passive eavesdropping, not against an active attacker who
  can intercept your first connection: compare the fingerprint the container logs (and `make cert` prints) with what
  the device shows before you trust it, or use your own certificate / a reverse proxy with a public one ([install.md](install.md)).
  HSTS is only sent when `RUSTBUCKET_BASE_URL` is an https URL, so the self-signed test setup does not pin `localhost`.
- **Dependency and image scanning:** `make audit-deps` runs `cargo audit` (RustSec advisories) and `cargo deny` (advisories,
  licenses, bans, sources; policy in `deny.toml`); `make scan-image` runs Trivy over the built image (Alpine packages incl. nginx,
  openssl, curl, and secrets in layers) and fails on fixable HIGH/CRITICAL findings. CI runs both daily, because advisories
  appear without any code change. The crate itself forbids `unsafe` (`#![forbid(unsafe_code)]`). SonarQube does not analyze Rust;
  it covers the JavaScript and the templates.
- **Not covered:** rate limiting beyond logins (put a proxy in front for that), server-side encryption at rest (use disk encryption),
  malware scanning, audit log of user actions beyond the request log.

Report problems to the maintainer; do not post exploits publicly.
