FROM rust:1.97.1-slim-bookworm@sha256:2775a09d208ff0d7c1f50490c45b62db929e87ba1dcbc3f2132ac71a704bcdd3 AS build

ARG BORINGBUILDER_CACHE_EPOCH=release
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake perl pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && test -n "${BORINGBUILDER_CACHE_EPOCH}"

WORKDIR /src
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY src src
RUN --mount=type=cache,id=boringbuilder-cargo-registry-${BORINGBUILDER_CACHE_EPOCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=boringbuilder-cargo-git-${BORINGBUILDER_CACHE_EPOCH},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=boringbuilder-cargo-target-${BORINGBUILDER_CACHE_EPOCH},target=/src/target,sharing=locked \
    cargo build --locked --release \
    && install -D -m 0755 target/release/boringbuilder /out/boringbuilder

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

ARG BORINGBUILDER_CACHE_EPOCH=release
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && test -n "${BORINGBUILDER_CACHE_EPOCH}"

COPY --from=build /out/boringbuilder /usr/local/bin/boringbuilder

ENTRYPOINT ["/usr/local/bin/boringbuilder"]
CMD ["--help"]
