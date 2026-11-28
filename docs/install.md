# Installing rustbucket (one container)

The default image bundles everything: **nginx with a self-signed ("snakeoil") certificate in front of the server**,
one container, one published port (8443, TLS). No other service, no reverse proxy, no certificate authority needed.

> Public preview / test build: good for trying it out and for testing with the Nextcloud clients. Read
> [security.md](security.md) and keep backups before you put data in it that you cannot lose.

## Run it
```
git clone <this repository> && cd rustbucket
make run            # builds the image on first use, starts the container, https://localhost:8443/
make password       # the generated password of the first administrator ("admin"); shown once, in the log
```
Open `https://localhost:8443/` and accept the certificate warning (or trust the certificate: [ca-rollout.md](ca-rollout.md)).
In a Nextcloud client use the server address `https://localhost:8443` (or the name you put in the certificate, below).

Without `make`: `docker build -t rustbucket:preview . && docker run -d --name rustbucket -p 8443:8443 -v rustbucket-data:/data -v rustbucket-db:/db -v rustbucket-certs:/certs rustbucket:preview`.

## Reach it from other devices
The certificate must contain the name or address the client types. Give them at start (a changed list renews the certificate):
```
make run BIND=0.0.0.0 TLS_NAMES=localhost,127.0.0.1,files.example.com,192.0.2.10 PORT=8443
```
`BIND=0.0.0.0` publishes the port on every interface; the default (127.0.0.1) is reachable from this machine only.
Then trust the certificate on each device: [ca-rollout.md](ca-rollout.md). `make cert` writes it to `rustbucket-cert.pem`.

## Container settings
| Variable | Default | Meaning |
|---|---|---|
| `RB_TLS_NAMES` | `localhost,127.0.0.1` | DNS names / IP addresses for the self-signed certificate |
| `RB_TLS_DAYS` | `825` | validity of a generated certificate (it is renewed 30 days before the end) |
| `RB_ADMIN_USER` | `admin` | name of the administrator created on the first start (only when there is no user yet) |
| `RB_ADMIN_PASSWORD` / `RB_ADMIN_PASSWORD_FILE` | generated | its password (the file form keeps it out of `docker inspect`); generated and logged once when neither is set |

Volumes: `/data` (the files), `/db` (the index), `/certs` (certificate + key). To use your own certificate, put
`tls.crt` and `tls.key` into `/certs`; a certificate you supplied is never touched.

Users: `make user U=bob [ADMIN=1] [FULLNAME="Bob B"]` (the password is asked for and goes through stdin).
More: [admin-cli.md](admin-cli.md).

## Behind your own reverse proxy instead
Build the bare server (`docker build --target server -t rustbucket:server .`) and let your proxy terminate TLS:
the server speaks plain HTTP on 8080. Set `RUSTBUCKET_TRUSTED_PROXIES` to the proxy's address range (its
`X-Forwarded-*` headers are trusted only from there) or `RUSTBUCKET_BASE_URL=https://public.name`, and make the
proxy stream uploads (no request buffering, no body-size limit); see `docker/nginx.conf` for a working example.
The live editor talks over a **WebSocket** (`/apps/files/edit/ws`): the proxy has to pass `Upgrade` and `Connection` on (nginx:
`proxy_set_header Upgrade $http_upgrade;` plus a `map` for `Connection`, as in `docker/nginx.conf`; Traefik and Caddy do it by default)
and keep idle connections for a while (`proxy_read_timeout`). Without it the editor shows "offline, reconnecting"; everything else works.

## Server configuration (environment variables, or a TOML file with the same keys in lower case)
| Variable | Default | Meaning |
|---|---|---|
| `RUSTBUCKET_BASE_URL` | none | public URL; used for links and to decide `Secure` cookies and HSTS (`https://...`) |
| `RUSTBUCKET_TRUSTED_PROXIES` | none (`127.0.0.1/32` in the standalone image) | comma separated CIDRs whose `X-Forwarded-*` headers are trusted |
| `RUSTBUCKET_LISTEN` | `0.0.0.0:8080` (`127.0.0.1:8080` in the standalone image) | bind address |
| `RUSTBUCKET_DATABASE_URL` | `sqlite:///db/rustbucket.db` | SQLite file (WAL) |
| `RUSTBUCKET_DATA_DIR` | `/data` | the files (`<user>/files`, `trash`, `versions`, `uploads`, `tmp`; `.appdata` = preview cache) |
| `RUSTBUCKET_MAX_FILE_SIZE` | `1073741824` | largest file in bytes (0 = unlimited); applies to plain and chunked uploads |
| `RUSTBUCKET_TRASH_RETENTION_DAYS` | `30` | days a deleted item stays in the trash (0 = until emptied) |
| `RUSTBUCKET_VERSION_RETENTION_DAYS` | `90` | days an old version is kept (0 = no age limit) |
| `RUSTBUCKET_VERSIONS_MAX` | `50` | old versions kept per file |
| `RUSTBUCKET_METRICS_TOKEN` | unset | enables `GET /metrics` for `Authorization: Bearer <token>` (404 while unset) |
| `RUSTBUCKET_EDITOR` | `true` | the live editor for text files (and direct editing for the mobile apps); `false` answers 404 |
| `RUSTBUCKET_EDITOR_MAX_BYTES` | `2097152` | largest text file the editor opens or keeps; bigger files are refused with a message |
| `RUSTBUCKET_LOG_JSON` | `true` | JSON log lines |
| `RUSTBUCKET_AUTO_MIGRATE` | `true` | run migrations on start |

Pass them to the container with `docker run -e NAME=value ...` (`make run` does not forward them; use `docker run` directly or edit the Makefile).

## Layout on disk (inside the volumes)
```
/db/rustbucket.db            the index (+ -wal/-shm while running; /db/backups/ holds `rustbucket backup` snapshots)
/data/.rustbucket-data       marker
/data/<user>/files/...       the files, under their real names (back these up, they are the truth)
/data/<user>/trash/<id>/     trashed items            /data/<user>/versions/<fileid>/<mtime>   old versions (hard links)
/data/<user>/uploads|tmp/    chunked uploads in flight / temporary files (cleaned up automatically)
/data/.appdata/preview/      preview cache (can be deleted any time)
```
On first start the server writes the marker; if it is missing while the database has files (wrong or unmounted
data volume), the server **refuses to start** instead of serving an empty tree.
