# boringbuilder

`boringbuilder` is a fast, native artifact and OCI image builder. It executes a
recipe or Dockerfile, caches the filesystem changes, and exports the result. It
does not require Docker.

```sh
cargo run --release -- build -f boringbuilder.yml --platform linux/arm64
```

That is the product: source plus build instructions in, portable artifact or OCI
image out.

## How it executes builds

- On macOS, Linux builds run with Apple Container. Native macOS recipes can opt
  into `runtime: host`.
- On Linux, builds use overlay mounts and chroot directly. Docker and a nested
  container runtime are unnecessary. Chroot is an execution mechanism, not a
  security boundary, so this backend is intended for trusted builds on
  disposable or otherwise isolated machines.

## Build an artifact

```yaml
# boringbuilder.yml
image: alpine:3.21
platform: linux/arm64
workdir: /src

inputs:
  - source: .
    dest: /src
    readonly: true

steps:
  - name: package
    run: |
      mkdir -p /out
      cp /src/my-app /out/my-app

outputs:
  - /out

export:
  format: tar.zst
  path: ./dist/my-app-linux-arm64.tar.zst
```

```sh
boringbuilder build
boringbuilder build --platform linux/amd64
boringbuilder build --dry-run
```

## Build an OCI image from a Dockerfile

```sh
boringbuilder build -f Dockerfile \
  --platform linux/arm64 \
  --format oci \
  -o ./dist/image.oci
```

Push without Docker by naming the image directly:

```sh
boringbuilder build -f Dockerfile \
  --platform linux/arm64 \
  --push ghcr.io/acme/my-app:latest
```

The supported exports are OCI layout, Docker archive, `tar.zst`, and tar.
Use `--target NAME` for a multi-target recipe and `--build-arg KEY=VALUE` for
Dockerfile or recipe variables.

```yaml
targets:
  compile:
    image: alpine:3.21
    steps:
      - run: mkdir -p /out && cp /src/my-app /out/my-app
    outputs: [/out]

  package:
    image: alpine:3.21
    needs: [compile]
    steps:
      - run: cp /boringbuilder-stages/compile/out/my-app /package
    outputs: [/package]
    export:
      format: tar.zst
      path: ./dist/my-app.tar.zst
```

## Cache

Local caching is enabled by default under `~/.boringbuilder/cache`.

```sh
boringbuilder build                         # local cache
boringbuilder build --no-cache              # cold build
boringbuilder build --cache-explain         # explain reuse
boringbuilder build --cache boringcache \
  --cache-workspace acme/project
boringbuilder build --cache ghcr.io/acme/build-cache
```

For a cross-platform macOS build using BoringCache, pass a Linux BoringCache
binary with `--cache-bin` when one is not already discoverable locally.

## What it does

`boringbuilder` owns:

- native recipes and Dockerfile parsing;
- build-time execution;
- Linux and macOS execution backends;
- content-addressed build caching;
- artifact, OCI, and Docker-compatible exports;
- registry push; and
- platform targeting such as `linux/arm64`.

Everything is available through the single `boringbuilder build` command.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/boringcache/builder/main/install.sh | sh
```

Or build from source with `cargo install --path .`.

## Continuous integration

CI treats formatting, Clippy warnings, the full test suite, recipe planning, and
RustSec advisories as release gates. Dependabot maintains both Cargo crates and
GitHub Actions. Every third-party workflow action is pinned to an immutable
commit with least-privilege permissions.

Boringbuilder dogfoods BoringCache for Cargo dependency archives, target
snapshots, compiler outputs, and release artifacts. The same cache plan lives
in `.boringcache.toml`, so local and hosted builds share one configuration.

## Releases

Releases use SemVer names. We start with `v0.1.0-alpha.1`, while the same
pipeline supports `beta`, `rc`, other prerelease suffixes, and stable releases.
Run the `Boringbuilder Release` workflow manually from `main` and enter the
version. The workflow validates it, runs formatting, Clippy, tests, and the
dependency audit, builds every supported binary, publishes checksums, and marks
versions with a prerelease suffix as GitHub prereleases.

Stable `vX.Y.Z` tags use the same gates and artifact pipeline. Install any
specific stable or alpha release with:

```sh
curl -fsSL https://raw.githubusercontent.com/boringcache/builder/main/install.sh | \
  BORINGBUILDER_VERSION=v0.1.0-alpha.1 sh
```

## Develop

Rust 1.94.1 is pinned in `mise.toml`.

```sh
mise install
make verify
cargo run -- build -f examples/artifact.yml --dry-run
```

See `boringbuilder build --help` for the complete, intentionally small CLI.
