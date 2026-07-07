# syntax=docker/dockerfile:1

# Build stage. Only the shop is containerized; the issuer is an offline CLI you
# run on a trusted/air-gapped box, not a service. sqlx talks to SQLite over a
# statically-compiled bundled libsqlite3 (needs a C toolchain, present in the
# rust image) and to Stripe over rustls, so there's no libpq or OpenSSL here.
FROM rust:1-slim-trixie AS builder
WORKDIR /app
COPY . .
# BuildKit cache mounts keep the cargo registry and target dir warm across
# builds; the binary is copied out of the cached target in the same layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin signet-shop \
    && cp target/release/signet-shop /usr/local/bin/signet-shop

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /usr/local/bin/signet-shop /app/signet-shop

# Listen on all interfaces inside the container (the default is loopback-only).
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

# Mount catalog.toml, content/, keys/, and a data volume for the SQLite DB at
# runtime, e.g.:
#   -v ./catalog.toml:/app/catalog.toml:ro -v ./keys:/app/keys:ro
#   -v signet-data:/data -e DATABASE_URL=sqlite:///data/signet.db
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD curl -sf http://localhost:8080/ || exit 1

CMD ["./signet-shop", "serve"]
