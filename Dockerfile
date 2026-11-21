# syntax=docker/dockerfile:1
# Alpine (musl) image, two stages that matter: "server" = the bare server (plain HTTP on :8080), "standalone" (the default,
# last stage) = nginx with a self-signed certificate in front of it. The former Debian Dockerfile is shelved in docs/shelf/.
FROM rust:1-alpine@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS build
# rust:alpine builds for x86_64-unknown-linux-musl; libsqlite3-sys compiles SQLite from C, which needs gcc and musl headers
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
# Dependency layer: build with a stub main so source changes do not rebuild crates.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release && rm -rf src target/release/deps/rustbucket* target/release/rustbucket*
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
COPY src ./src
# copied files keep old mtimes; force cargo to rebuild the crate over the stub
RUN touch src/main.rs src/lib.rs && cargo build --release

# --- stage "server": the bare server (plain HTTP on :8080)
FROM alpine:3@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf AS server
# security updates at build time (the pinned base is rebuilt only now and then); the date argument re-runs this layer daily
ARG PKG_DATE=none
RUN echo "package updates of $PKG_DATE" && apk upgrade --no-cache
RUN adduser -S -D -H -u 10001 -s /sbin/nologin rustbucket \
    && mkdir /data /db && chown rustbucket /data /db
COPY --from=build /src/target/release/rustbucket /usr/local/bin/rustbucket
ARG BUILD_DATE=unknown
ARG GIT_COMMIT=unknown
ENV RUSTBUCKET_BUILD_DATE=${BUILD_DATE} RUSTBUCKET_BUILD_COMMIT=${GIT_COMMIT}
LABEL org.opencontainers.image.created=${BUILD_DATE} org.opencontainers.image.revision=${GIT_COMMIT}
USER rustbucket
ENV RUSTBUCKET_DATA_DIR=/data RUSTBUCKET_DATABASE_URL=sqlite:///db/rustbucket.db
VOLUME ["/data", "/db"]
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 CMD ["rustbucket", "healthcheck"]
ENTRYPOINT ["rustbucket"]
CMD ["serve"]

# --- stage "standalone" (the default target): nginx with a self-signed certificate in front of the server
FROM server AS standalone
USER root
# bash: the entrypoint uses arrays and `wait -n`; GNU tar: `make backup` needs --transform/--exclude, which busybox tar lacks
RUN apk add --no-cache nginx openssl curl bash tar \
    && rm -f /etc/nginx/http.d/* \
    && mkdir /certs && chown rustbucket /certs
COPY docker/nginx.conf /etc/nginx/nginx.conf
COPY docker/entrypoint.sh /usr/local/bin/rustbucket-standalone
COPY docker/healthcheck.sh /usr/local/bin/rustbucket-healthcheck
RUN chmod 755 /usr/local/bin/rustbucket-standalone /usr/local/bin/rustbucket-healthcheck
USER rustbucket
ENV RUSTBUCKET_LISTEN=127.0.0.1:8080 RUSTBUCKET_TRUSTED_PROXIES=127.0.0.1/32
VOLUME ["/data", "/db", "/certs"]
EXPOSE 8443
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 CMD ["rustbucket-healthcheck"]
ENTRYPOINT ["rustbucket-standalone"]
CMD ["run"]
