# rustbucket - public preview. Everything runs in docker: no Rust toolchain on the host is needed.
# `make` (or `make help`) lists the targets. AI agents: read CLAUDE.md first.

IMAGE     ?= rustbucket:preview
NAME      ?= rustbucket
PORT      ?= 8443
# address the port is published on: localhost only by default; BIND=0.0.0.0 to serve other devices
BIND      ?= 127.0.0.1
# names/addresses clients will use to reach the server; they go into the self-signed certificate
TLS_NAMES ?= localhost,127.0.0.1
RUST_IMAGE ?= rust:1
# build number stamped into the image: UTC date and short commit ("-dirty" with uncommitted changes; "dev" without git);
# PKG_DATE makes the Alpine security-update layer re-run every day (see the Dockerfile)
BUILD_ARGS = --build-arg PKG_DATE=$$(date -u +%Y%m%d) --build-arg BUILD_DATE=$$(date -u +%Y-%m-%dT%H:%M:%SZ) --build-arg GIT_COMMIT=$$(git describe --always --dirty --abbrev=7 2>/dev/null || echo unknown)

.DEFAULT_GOAL := help
.PHONY: help image run start stop restart rm logs status shell password user cert purge backup restore smoke \
        test fmt clippy check audit-pii audit-deps scan-image e2e version bump-version

help:
	@echo "run it"
	@echo "  make image            build the image ($(IMAGE)): the server + nginx with a self-signed certificate, one container"
	@echo "  make run              build if needed, start the container '$(NAME)' on https://localhost:$(PORT)/"
	@echo "                        [BIND=127.0.0.1 (0.0.0.0 = reachable from other devices)] [PORT=8443] [TLS_NAMES=localhost,127.0.0.1,files.example.com,192.0.2.10] [NAME=rustbucket]"
	@echo "  make password         show the generated administrator password (first start only)"
	@echo "  make user U=bob [ADMIN=1] [FULLNAME='Bob B']   create a user (the password is asked for, never on a command line)"
	@echo "  make cert             write the certificate to ./rustbucket-cert.pem and print its fingerprint"
	@echo "  make logs | status | shell | stop | start | restart | rm"
	@echo "  make backup [DEST=./backups]          consistent backup (database snapshot first, then the files) as one .tar.gz"
	@echo "  make restore FILE=backups/x.tar.gz    restore into EMPTY volumes (new host, or after a purge), then start"
	@echo "  make purge CONFIRM=1  remove the container AND its volumes (all files and users!)"
	@echo "check it"
	@echo "  make smoke            build, start a throw-away container, probe it over TLS, remove it"
	@echo "  make check            fmt + clippy + all tests (inside a rust container)    [make test | fmt | clippy]"
	@echo "  make version          the version in Cargo.toml and the commit the next image is stamped with"
	@echo "  make bump-version V=0.2.0|patch|minor|major   change the version (needs python3 on the host; commits nothing)"
	@echo "  make audit-deps       known vulnerabilities in the Rust dependencies: cargo audit (RustSec) + cargo deny (advisories, licenses, bans, sources)"
	@echo "  make scan-image       CVEs in the built image (Alpine packages incl. nginx/openssl/curl) with Trivy; fails on fixable HIGH/CRITICAL"
	@echo "  make audit-pii        fail if the tree contains private addresses, home paths, or real-looking emails"
	@echo "  make e2e              real Nextcloud sync client against a throw-away server (needs 'nextcloudcmd' on the host)"

image:
	docker build $(BUILD_ARGS) -t $(IMAGE) .

version:
	@grep -m1 '^version' Cargo.toml; git describe --always --dirty --abbrev=7 2>/dev/null || true

# needs python3 on the host; changes Cargo.toml + Cargo.lock + fuzz/Cargo.lock, commits nothing
bump-version:
	@test -n "$(V)" || { echo "usage: make bump-version V=0.2.0 | V=patch|minor|major"; exit 2; }
	python3 scripts/bump_version.py $(V)

run:
	@docker image inspect $(IMAGE) >/dev/null 2>&1 || $(MAKE) image
	@docker container inspect $(NAME) >/dev/null 2>&1 && { echo "container '$(NAME)' exists: make start / make restart, or make rm first"; exit 1; } || true
	docker run -d --name $(NAME) --restart unless-stopped -p $(BIND):$(PORT):8443 \
	  -v $(NAME)-data:/data -v $(NAME)-db:/db -v $(NAME)-certs:/certs \
	  -e RB_TLS_NAMES='$(TLS_NAMES)' $(IMAGE)
	@echo "starting: https://localhost:$(PORT)/  (self-signed certificate: accept the warning, or 'make cert' and trust it)"
	@echo "first administrator: 'make password'"

start stop restart:
	docker $@ $(NAME)
rm:
	docker rm -f $(NAME)
logs:
	docker logs --tail 100 -f $(NAME)
status:
	@docker ps -a --filter name='^$(NAME)$$' --format '{{.Names}}\t{{.Status}}\t{{.Ports}}'
shell:
	docker exec -it $(NAME) bash

password:
	@docker logs $(NAME) 2>&1 | grep -m1 'ADMIN_PASSWORD=' | sed 's/.*ADMIN_PASSWORD=/admin password: /' \
	  || echo "no generated password in the log (it is only generated on the first start, and not when RB_ADMIN_PASSWORD was set)"

# the password goes through stdin (docker exec -i), so it is not visible in the process list
user:
	@test -n "$(U)" || { echo "usage: make user U=bob [ADMIN=1] [FULLNAME='Bob B']"; exit 2; }
	@read -r -s -p "Password for $(U): " pw; echo; read -r -s -p "Again: " pw2; echo; \
	[ -n "$$pw" ] && [ "$$pw" = "$$pw2" ] || { echo "passwords empty or different"; exit 1; }; \
	printf '%s\n' "$$pw" | docker exec -i $(NAME) rustbucket user create '$(U)' $(if $(filter-out 0 no false,$(ADMIN)),--admin) $(if $(FULLNAME),--display-name '$(FULLNAME)')

cert:
	docker cp $(NAME):/certs/tls.crt ./rustbucket-cert.pem
	@openssl x509 -noout -subject -enddate -fingerprint -sha256 -in rustbucket-cert.pem

purge:
	@test "$(CONFIRM)" = 1 || { echo "this deletes all files, users and the certificate: make purge CONFIRM=1"; exit 1; }
	-docker rm -f $(NAME)
	-docker volume rm $(NAME)-data $(NAME)-db $(NAME)-certs

DEST ?= ./backups
STAMP = $(shell date +%Y%m%d-%H%M%S)
# 1. online snapshot of the database (safe while running), 2. then the data. A data copy newer than the snapshot is
# repaired by `scan` after a restore. tar keeps hard links (old versions); preview cache and in-flight uploads are left out.
# The archive is streamed to the host, so it is owned by you. The temporary snapshot (in the container's /tmp,
# gone when the container is recreated) is emptied afterwards.
backup:
	@mkdir -p '$(DEST)'
	docker exec $(NAME) rustbucket backup /tmp/snap-$(STAMP).db
	docker exec $(NAME) tar -czf - --exclude=data/.appdata --exclude='data/*/tmp' --exclude='data/*/uploads' \
	  --transform 's#^tmp/snap-$(STAMP).db$$#db/rustbucket.db#' -C / tmp/snap-$(STAMP).db data > '$(DEST)/rustbucket-$(STAMP).tar.gz'
	docker exec $(NAME) truncate -s 0 /tmp/snap-$(STAMP).db
	@echo "written: $(DEST)/rustbucket-$(STAMP).tar.gz"

# Never overwrites anything: it only fills EMPTY volumes (new host, or after `make purge CONFIRM=1`), then starts.
restore:
	@test -f '$(FILE)' || { echo "usage: make restore FILE=backups/rustbucket-<stamp>.tar.gz"; exit 2; }
	@docker image inspect $(IMAGE) >/dev/null 2>&1 || $(MAKE) image
	@! docker container inspect $(NAME) >/dev/null 2>&1 || { echo "container '$(NAME)' exists: restore needs a clean start (make purge CONFIRM=1 deletes it and its volumes)"; exit 1; }
	docker volume create $(NAME)-data >/dev/null && docker volume create $(NAME)-db >/dev/null && docker volume create $(NAME)-certs >/dev/null
	docker run --rm --entrypoint sh -v $(NAME)-db:/db:ro -v $(NAME)-data:/data:ro $(IMAGE) -c \
	  'test -z "$$(ls -A /data)$$(ls -A /db)"' || { echo "the volumes $(NAME)-data / $(NAME)-db are not empty: refusing to mix (make purge CONFIRM=1 first)"; exit 1; }
	docker run --rm -i --entrypoint tar -v $(NAME)-db:/db -v $(NAME)-data:/data $(IMAGE) -xzf - -C / < '$(FILE)'
	$(MAKE) run
	@for i in $$(seq 1 60); do docker exec $(NAME) rustbucket healthcheck >/dev/null 2>&1 && break; sleep 1; done
	docker exec $(NAME) rustbucket scan --dry-run
	@echo "restored. If the scan above lists differences (files changed after the snapshot): docker exec $(NAME) rustbucket scan"

smoke:
	scripts/smoke.sh

# cargo runs in a container (sources mounted, build cache in docker volumes); nothing is written to the repo
CARGO = docker run --rm -v "$(CURDIR)":/work -w /work -v rustbucket-cargo-registry:/usr/local/cargo/registry \
        -v rustbucket-cargo-target:/target -e CARGO_TARGET_DIR=/target $(RUST_IMAGE)
test:
	$(CARGO) cargo test
fmt:
	$(CARGO) sh -c 'rustup component add rustfmt >/dev/null 2>&1; cargo fmt'
clippy:
	$(CARGO) sh -c 'rustup component add clippy >/dev/null 2>&1; cargo clippy --all-targets -- -D warnings'
check:
	$(CARGO) sh -c 'rustup component add rustfmt clippy >/dev/null 2>&1; cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'

audit-pii:
	scripts/audit_pii.sh

# Dependency scan. cargo-audit and cargo-deny are built once into a docker volume (first run takes a few minutes).
# New advisories appear without any code change, so CI runs this daily as well. Policy: a vulnerability fails the run,
# "unmaintained" notices are shown but do not (see deny.toml for the rest).
TOOLS = docker run --rm -v "$(CURDIR)":/work -w /work -v rustbucket-cargo-registry:/usr/local/cargo/registry \
        -v rustbucket-cargo-tools:/tools -e CARGO_INSTALL_ROOT=/tools -e CARGO_TARGET_DIR=/tmp/tools-target \
        -e PATH=/tools/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin $(RUST_IMAGE)
audit-deps:
	$(TOOLS) sh -c 'for t in cargo-audit cargo-deny; do command -v $$t >/dev/null || cargo install $$t --locked || exit 1; done; cargo audit && cargo deny check'

# Image scan with Trivy (pinned). The image is streamed into the scanner's own container: nothing is written to the host
# and the docker socket is not handed to a third-party image. Fixed HIGH/CRITICAL CVEs and secrets in layers fail the run.
TRIVY_IMAGE ?= aquasec/trivy:0.76.2
SCAN_IMAGE  ?= $(IMAGE)
scan-image:
	@docker image inspect $(SCAN_IMAGE) >/dev/null 2>&1 || $(MAKE) image
	docker save $(SCAN_IMAGE) | docker run --rm -i -v rustbucket-trivy-cache:/root/.cache --entrypoint sh $(TRIVY_IMAGE) -c \
	  'cat > /tmp/image.tar && trivy image --input /tmp/image.tar --severity HIGH,CRITICAL --ignore-unfixed --scanners vuln,secret --exit-code 1'

e2e:
	scripts/e2e.sh
