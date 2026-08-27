# Changelog

## 0.1.0.alpha.3 - 2026-08-27

- Remove Bundler cache metadata before compiling assets so cold and restored Rails builds stay identical.

## 0.1.0.alpha.2 - 2026-08-27

- Stream clean, Docker-style build steps and command output through Dagger Ruby.
- Run Bundler with the available CPUs unless `BUNDLE_JOBS` is explicitly configured.
- Keep dependency cache metadata out of Rails artifacts.

## 0.1.0.alpha.1 - 2026-08-27

- Reintroduce BoringBuilder as a Ruby gem backed by Dagger Ruby.
- Add a convention-driven production Rails build.
- Support Apple Container and Docker Dagger runners.
- Support selective directory, tar, tar.zst, OCI, Docker, runtime-load, and registry-push outputs.
- Add local and BoringCache Artifact exporter adapters with automatic shared publication.
- Add cache-aware custom Dagger pipeline steps backed by local mounts and optional BoringCache persistence.
- Add a shared Mise toolchain foundation and `boringbuilder init` scaffolds for Ruby, Node, Rust, Go, and generic
  Dagger pipelines.
- Add a Ruby API, CLI, Rails rake tasks, tests, CI, dependency auditing, and trusted RubyGems publishing.
