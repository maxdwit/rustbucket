#!/bin/bash
# Entry point of the standalone image.
#   run (default)   prepare certificate and first administrator, then run the server and nginx
#   anything else   is passed to the admin CLI, e.g.  docker run --rm -v ... IMAGE user list
set -eu

if [ "${1:-run}" != run ]; then exec rustbucket "$@"; fi

log() { echo "[standalone] $*" >&2; }
CERT_DIR=/certs
CRT="$CERT_DIR/tls.crt"; KEY="$CERT_DIR/tls.key"; MARK="$CERT_DIR/.generated"

# ---- TLS: use the certificate that was mounted, otherwise make (and keep) a self-signed one ("snakeoil")
san_list() {  # "a.example,192.0.2.5" -> "DNS:a.example,IP:192.0.2.5"
  local out="" n kind
  IFS=, read -ra names <<< "${RB_TLS_NAMES:-localhost,127.0.0.1}"
  for n in "${names[@]}"; do
    n="${n// /}"; [ -n "$n" ] || continue
    if [[ "$n" =~ ^[0-9]+(\.[0-9]+){3}$ || "$n" == *:* ]]; then kind=IP; else kind=DNS; fi
    out="$out,$kind:$n"
  done
  echo "${out#,}"
}
make_cert() {
  local san cn tmp; san="$(san_list)"
  [ -n "$san" ] || { log "RB_TLS_NAMES is empty"; exit 1; }
  cn="${san%%,*}"; cn="${cn#*:}"
  tmp="$(mktemp -d)"; umask 077
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout "$tmp/key" -out "$tmp/crt" -days "${RB_TLS_DAYS:-825}" -subj "/CN=$cn" \
    -addext "subjectAltName=$san" -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature" -addext "extendedKeyUsage=serverAuth" 2>/dev/null
  mv "$tmp/key" "$KEY"; mv "$tmp/crt" "$CRT"; rmdir "$tmp"
  printf '%s\n' "$san" > "$MARK"
  log "generated a self-signed certificate for: $san"
}
if [ ! -s "$CRT" ] || [ ! -s "$KEY" ]; then
  make_cert
elif [ -e "$MARK" ]; then   # ours: renew when the names changed or it expires within 30 days (a mounted one is never touched)
  if [ "$(cat "$MARK")" != "$(san_list)" ] || ! openssl x509 -checkend 2592000 -noout -in "$CRT" >/dev/null; then make_cert; fi
fi
log "certificate SHA-256 fingerprint: $(openssl x509 -noout -fingerprint -sha256 -in "$CRT" | cut -d= -f2)"

# ---- database and first administrator
rustbucket migrate >/dev/null
if [ -z "$(RUST_LOG=off rustbucket user list 2>/dev/null)" ]; then
  admin="${RB_ADMIN_USER:-admin}"; generated=0
  if [ -n "${RB_ADMIN_PASSWORD_FILE:-}" ]; then pw="$(head -n1 "$RB_ADMIN_PASSWORD_FILE")"
  elif [ -n "${RB_ADMIN_PASSWORD:-}" ]; then pw="$RB_ADMIN_PASSWORD"
  else pw="$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)"; generated=1; fi
  printf '%s\n' "$pw" | rustbucket user create "$admin" --admin >/dev/null
  if [ "$generated" = 1 ]; then
    log "first start: created administrator '$admin' with the generated password below (shown once):"
    log "ADMIN_PASSWORD=$pw"
  else
    log "first start: created administrator '$admin'"
  fi
fi

# ---- run both; stop both as soon as one of them ends or a stop signal arrives
mkdir -p /tmp/nginx
rustbucket serve & rb=$!
for _ in $(seq 1 100); do curl -fs --max-time 2 http://127.0.0.1:8080/health >/dev/null 2>&1 && break; sleep 0.3; done
nginx -g 'daemon off;' & ng=$!
trap 'kill -TERM $rb $ng 2>/dev/null || true' TERM INT
log "ready: https://<host>:8443/"
set +e
wait -n $rb $ng; code=$?
kill -TERM $rb $ng 2>/dev/null; wait
exit $code
