# syntax=docker/dockerfile:1

FROM rust:1.99.1-alpine3.24 AS chef
WORKDIR /build
RUN cargo install cargo-chef --locked --version 0.1.78

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Keep compiled dependencies in this layer: external BuildKit caches export
# layers, whereas cache mounts alone do not survive an ephemeral CI runner.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --locked --package brscanr --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked --package brscanr --bin brscanr \
    && mkdir -p /out/spool \
    && cp target/release/brscanr /out/brscanr \
    && readelf -l /out/brscanr > /out/program-headers \
    && ! grep -q INTERP /out/program-headers

FROM gcr.io/distroless/static-debian13:nonroot
COPY --from=builder /out/brscanr /brscanr
COPY --from=builder --chown=65532:65532 /out/spool /var/lib/brscanr
USER 65532:65532
EXPOSE 8080/tcp 54925/udp
ENTRYPOINT ["/brscanr"]
