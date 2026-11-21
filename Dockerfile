# syntax=docker/dockerfile:1
FROM rust:1@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8 AS build
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

# --- stage "server": the bare server (plain HTTP on :8080). Used by the test scripts (--target server).
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS server
RUN useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin rustbucket \
    && mkdir /data /db && chown rustbucket /data /db
COPY --from=build /src/target/release/rustbucket /usr/local/bin/rustbucket
# build number: `make image` passes both (UTC date and short git commit); without them the server shows "dev" (kept
# after the COPY so the expensive layers stay cached)
ARG BUILD_DATE=unknown
ARG GIT_COMMIT=unknown
ENV RUSTBUCKET_BUILD_DATE=${BUILD_DATE} RUSTBUCKET_BUILD_COMMIT=${GIT_COMMIT}
LABEL org.opencontainers.image.created=${BUILD_DATE} org.opencontainers.image.revision=${GIT_COMMIT}
USER rustbucket
ENV RUSTBUCKET_DATA_DIR=/data RUSTBUCKET_DATABASE_URL=sqlite:///db/rustbucket.db
VOLUME ["/data", "/db"]
# (no EXPOSE here: the bare server listens on :8080; the standalone image below exposes only its TLS port)
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 CMD ["rustbucket", "healthcheck"]
ENTRYPOINT ["rustbucket"]
CMD ["serve"]

# --- stage "standalone" (the default): nginx with a self-signed ("snakeoil") certificate in front of the
# server, both in this one container, both unprivileged. TLS on :8443; everything else stays inside.
FROM server AS standalone
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends nginx openssl curl \
    && rm -rf /var/lib/apt/lists/* /etc/nginx/sites-enabled/* /etc/nginx/conf.d/* \
    && mkdir /certs && chown rustbucket /certs
COPY docker/nginx.conf /etc/nginx/nginx.conf
COPY docker/entrypoint.sh /usr/local/bin/rustbucket-standalone
COPY docker/healthcheck.sh /usr/local/bin/rustbucket-healthcheck
RUN chmod 755 /usr/local/bin/rustbucket-standalone /usr/local/bin/rustbucket-healthcheck
USER rustbucket
# the server listens on loopback only; the one trusted proxy is the nginx next to it
ENV RUSTBUCKET_LISTEN=127.0.0.1:8080 RUSTBUCKET_TRUSTED_PROXIES=127.0.0.1/32
VOLUME ["/data", "/db", "/certs"]
EXPOSE 8443
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 CMD ["rustbucket-healthcheck"]
ENTRYPOINT ["rustbucket-standalone"]
CMD ["run"]
