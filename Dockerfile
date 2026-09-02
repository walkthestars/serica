# syntax=docker/dockerfile:1
# Serica multi-stage Docker build
#
# Stage 1: chef — shared base for the two stages below. Installing
# cargo-chef here (rather than depending on the third-party
# lukemathwalker/cargo-chef image) keeps the build on the one base image,
# instead of introducing a second, independently-tagged image to track.
FROM rust:1.97.1-alpine3.22 AS chef
RUN apk add --no-cache musl-dev
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-chef --locked
WORKDIR /app

# Stage 2: planner — computes a recipe.json describing only the dependency
# graph (from Cargo.toml/Cargo.lock). This step is cheap and reruns on every
# source edit, but its output is not.
FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ src/
COPY tests/ tests/
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: builder — cooks (compiles) just the dependency graph from
# recipe.json first, as its own Docker layer keyed only on the recipe, so it
# is unaffected by and survives source-only edits. The real source is then
# copied in and built on top, with cache mounts for the cargo registry and
# the target dir so unchanged dependency artifacts are reused even when this
# layer itself has to re-run (e.g. after `COPY src/`).
#
# Parameterized on $TARGETARCH (set automatically by buildx to the
# platform currently being built — "amd64"/"arm64" — for each entry in the
# `platforms:` list in docker.yml) rather than hardcoding
# x86_64-unknown-linux-musl. Without this, a multi-arch build would still
# only ever produce an x86_64 binary, just tagged and pushed under both
# arch manifests — the arm64 manifest would fail immediately with an exec
# format error on real arm64 hardware. `rust-toolchain.toml` already
# declares both `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
# as targets for exactly this reason; `rustup target add` here is a no-op
# when the requested target is already the image's default host target
# (the common case under buildx's per-platform native/QEMU builds, where
# the arm64 build pulls the arm64 variant of the `rust:1.97.1-alpine3.22`
# base image and that's already its default host triple) and only does
# real work for genuine cross-builds.
FROM chef AS builder
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo "x86_64-unknown-linux-musl" > /rust_target.txt ;; \
      arm64) echo "aarch64-unknown-linux-musl" > /rust_target.txt ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac
RUN rustup target add "$(cat /rust_target.txt)"
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --features redis-cache --target "$(cat /rust_target.txt)" --recipe-path recipe.json
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ src/
COPY tests/ tests/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --features redis-cache --target "$(cat /rust_target.txt)" \
    && cp "target/$(cat /rust_target.txt)/release/serica" /serica

# Stage 4: Runtime
FROM alpine:3.22
# No curl: the container HEALTHCHECK below shells out to the `serica
# healthcheck` subcommand instead, so no network-capable binary beyond the
# app itself ships in the runtime image.
RUN apk add --no-cache ca-certificates \
    && addgroup -S serica && adduser -S -G serica serica
COPY --from=builder --chown=root:root --chmod=0755 \
     /serica /serica
USER serica
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/serica", "healthcheck"]
ENTRYPOINT ["/serica"]
CMD ["server"]
