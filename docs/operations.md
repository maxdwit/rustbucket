# Operations

- **Health:** `GET /health` (200 + `{"status":"ok"}` when the database answers) and `rustbucket healthcheck`;
  `GET /status.php` is the Nextcloud status document clients read first.
- **Logs:** one JSON line per request (method, uri, status, latency) and warnings/errors on stdout:
  `make logs` (`docker logs -f rustbucket`). No file contents or credentials are logged.
- **Metrics:** set `RUSTBUCKET_METRICS_TOKEN`, then `curl -H "Authorization: Bearer <token>" https://host/metrics`:
  Prometheus text with response counters by class, in-flight requests, request time, 401/403/429 count, uptime,
  users/files/shares/trash/sessions, free bytes on the data volume. Alert on `rustbucket_data_free_bytes`,
  a growing `rustbucket_http_responses_total{class="5xx"}` and a burst of `rustbucket_http_rejected_total`.
- **Disk space:** files + trash (default 30 days) + versions. Free space shows in `/metrics`; `rustbucket trash list <uid>`
  and `rustbucket trash empty <uid>` give it back; lower `RUSTBUCKET_TRASH_RETENTION_DAYS` / `VERSIONS_MAX` if needed.
- **Throttling:** 5 failed logins per client IP and 10 minutes, then 429 for the rest of the window (also for the web
  login and the password check of the settings page). Password hashing runs on at most 4 threads at a time.
- **Large uploads:** chunked upload (`/remote.php/dav/uploads`) keeps chunks under `data/<user>/uploads`; abandoned ones are
  removed after 24 hours. Your proxy must allow long requests and large bodies (no body size limit, generous timeouts).

## Troubleshooting
| Symptom | Look at |
|---|---|
| client says "connection problem" right after login | the proxy/CA: [ca-rollout.md](ca-rollout.md); `curl -v https://host/status.php` |
| server does not start: "data folder ... marker missing" | the data volume is not mounted (or a wrong one); fix the mount, do **not** create the marker by hand unless you are sure |
| files added by hand are not visible | `rustbucket scan <uid>` |
| a file is in the list but 404 on download | it vanished from disk: `rustbucket scan <uid>` |
| "too many failed attempts" (429) | wait 10 minutes; a device with an old password keeps failing - remove and re-add it |
| 413 on upload | `RUSTBUCKET_MAX_FILE_SIZE`, and the proxy's body limit |
