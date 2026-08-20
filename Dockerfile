FROM rust:1.94.1-slim-bookworm@sha256:cf9dd0ec73e75f827fe59123fff9dc65af1a1c8363c3c31ee8d7f8ad0b6a5fb2 AS build

ARG BORINGBUILDER_CACHE_EPOCH=release
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake perl pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && test -n "${BORINGBUILDER_CACHE_EPOCH}"

WORKDIR /src
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY src src
RUN cargo build --locked --release

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

ARG BORINGBUILDER_CACHE_EPOCH=release
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && test -n "${BORINGBUILDER_CACHE_EPOCH}"

COPY --from=build /src/target/release/boringbuilder /usr/local/bin/boringbuilder

ENTRYPOINT ["/usr/local/bin/boringbuilder"]
CMD ["--help"]
