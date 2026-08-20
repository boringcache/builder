# boringbuilder

[![CI](https://github.com/boringcache/builder/actions/workflows/ci.yml/badge.svg)](https://github.com/boringcache/builder/actions/workflows/ci.yml)
[![Security](https://github.com/boringcache/builder/actions/workflows/security.yml/badge.svg)](https://github.com/boringcache/builder/actions/workflows/security.yml)
[![CodeQL](https://github.com/boringcache/builder/actions/workflows/codeql.yml/badge.svg)](https://github.com/boringcache/builder/actions/workflows/codeql.yml)
[![Benchmarks](https://github.com/boringcache/builder/actions/workflows/benchmark.yml/badge.svg)](https://github.com/boringcache/builder/actions/workflows/benchmark.yml)

Fast native artifact and OCI image builds without Docker.

```sh
boringbuilder build -f Dockerfile --platform linux/arm64 --format oci -o dist/image.oci
```

Source plus build instructions go in. A portable artifact or image comes out.

## Install the alpha

The repository is private during the alpha, so installation uses your GitHub
login:

```sh
gh auth status
gh api repos/boringcache/builder/contents/install.sh \
  -H 'Accept: application/vnd.github.raw+json' | \
  sh -s -- --version v0.1.0-alpha.1
```

The installer verifies the release checksum and puts `boringbuilder` in
`~/.local/bin`. You can change that with `--install-dir`.

To build from source instead:

```sh
gh repo clone boringcache/builder
cd builder
cargo install --locked --path .
```

## Build an image

Use an ordinary Dockerfile:

```dockerfile
FROM alpine:3.21
RUN printf 'hello from boringbuilder\n' > /hello.txt
CMD ["cat", "/hello.txt"]
```

```sh
boringbuilder build -f Dockerfile \
  --platform linux/arm64 \
  --format oci \
  -o dist/image.oci
```

Push the result directly to a registry:

```sh
boringbuilder build -f Dockerfile \
  --platform linux/arm64 \
  --push ghcr.io/acme/my-app:latest
```

Docker is not used for either build. OCI layout, Docker archive, `tar.zst`,
and tar exports are supported.

## Build an artifact

A small YAML recipe is useful when you want files instead of an image:

```sh
boringbuilder build -f examples/artifact.yml --platform linux/arm64
```

See [examples/artifact.yml](examples/artifact.yml) for the complete recipe.
Use `--target NAME` for multi-target recipes and `--build-arg KEY=VALUE`
for Dockerfile or recipe variables.

## Cache expensive work

Local caching is on by default for recipe steps and Dockerfile `RUN`
instructions. Source changes invalidate the next cached build step automatically:

```sh
boringbuilder build
boringbuilder build --cache-explain
boringbuilder build --no-cache
```

Share build steps across ephemeral machines with BoringCache:

```sh
boringbuilder build \
  --cache boringcache \
  --cache-workspace acme/project
```

This repository dogfoods both cache layers. Multi-command validation jobs use
one BoringCache-managed sccache session. Release, benchmark, and self-build jobs
use one BoringCache Cargo lifecycle for registry, target, and compiler caches.

## How builds run

On macOS, Linux builds run through Apple Container. On Linux, boringbuilder
uses mounts and chroot directly on the host. Chroot is not a security boundary,
so the Linux backend is for trusted builds on disposable or otherwise isolated
machines.

Build-time execution is part of the builder; remote job orchestration is not.

## CI and releases

CI formats, lints, and tests the Rust project, then uses boringbuilder to build
its own Dockerfile twice, prove a warm cache hit with an identical image digest,
export OCI, push to a disposable registry, and pull every blob back through the
Registry API without Docker. Security CI runs RustSec, license and source
policy, and zizmor. Dependency review, CodeQL for Rust and Actions, and OpenSSF
Scorecard activate when the repository becomes public.
Dependabot maintains Cargo and workflow dependencies, and a scheduled workflow
records cold/warm wall time, peak memory, cache hits, and output digests.

The release workflow accepts any SemVer version, requires it to match
`Cargo.toml`, builds Linux amd64, Linux arm64, and Apple arm64 binaries,
publishes checksums, and verifies installation from the new GitHub release.
Versions such as `v0.1.0-alpha.1`, `v0.1.0-rc.1`, and `v0.1.0` all use
the same pipeline.

## Develop

Rust 1.98.0 is pinned in `mise.toml`.

```sh
mise install
make verify
cargo run -- build -f Dockerfile --platform linux/arm64 --dry-run
```

Run `boringbuilder build --help` for the intentionally small command surface.
