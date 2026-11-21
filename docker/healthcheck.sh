#!/bin/sh
# healthy = the whole chain answers: nginx (TLS) -> server
exec curl -fsSk --max-time 4 https://127.0.0.1:8443/health >/dev/null
