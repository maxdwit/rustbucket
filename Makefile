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

.DEFAULT_GOAL := help
.PHONY: help image run start stop restart rm logs status shell password user cert purge backup restore smoke \
        test fmt clippy check audit-pii e2e

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
	@echo "  make audit-pii        fail if the tree contains private addresses, home paths, or real-looking emails"
	@echo "  make e2e              real Nextcloud sync client against a throw-away server (needs 'nextcloudcmd' on the host)"

image:
	docker build -t $(IMAGE) .

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

e2e:
	scripts/e2e.sh
