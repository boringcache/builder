FROM rust:1.98.0-slim-bookworm@sha256:94e9efa4033213dbb70d4f665527e7ece3944ddb7ba1dd2e43f6fd6e2490af58 AS build

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
