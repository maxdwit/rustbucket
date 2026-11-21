#!/usr/bin/env bash
# Fails when a file of the tree (tracked, or new and not ignored) contains something that looks personal: private LAN addresses, home directories,
# private hostnames (.internal/.lan/.local/.home), or an email address that is not at an example domain.
# Run from anywhere:  scripts/audit_pii.sh        (make audit-pii)
set -u
cd "$(dirname "${BASH_SOURCE[0]}")/.."
self=scripts/audit_pii.sh
bad=0
scan() {  # description, regex (grep -E), optional filter-out regex
  local out
  out="$(git ls-files -z --cached --others --exclude-standard | xargs -0 grep -InE -- "$2" 2>/dev/null | grep -v "^$self:" | grep -vE "^(Cargo\.lock|fuzz/Cargo\.lock):" | { if [ -n "${3:-}" ]; then grep -vE -- "$3"; else cat; fi; })"
  if [ -n "$out" ]; then echo "== $1"; echo "$out" | cut -c1-200; bad=1; fi
}
scan "private LAN addresses" '(^|[^0-9.])(192\.168\.[0-9]+\.[0-9]+|10\.[0-9]+\.[0-9]+\.[0-9]+|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]+\.[0-9]+)' '(/[0-9]+|CIDR|trusted|proxy)'
scan "home directories / user paths" '(/home/[a-z0-9_-]+|/Users/[A-Za-z0-9_-]+|C:\\Users\\)'
scan "private hostnames" '[A-Za-z0-9-]+\.(internal|lan|local|home|corp|intranet)([^A-Za-z0-9_-]|$)'
scan "email addresses (only example.* domains and noreply are allowed)" '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' '@\.?(([a-z0-9-]+\.)*example\.(org|com|net|test)|noreply|users\.noreply\.github\.com|localhost)'
[ "$bad" = 0 ] && echo "audit-pii: clean" || { echo; echo "audit-pii: FOUND something above - replace it with an example value (alice, bob, example.com, 192.0.2.x)"; }
exit $bad
