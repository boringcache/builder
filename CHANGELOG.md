# Changelog

## 0.1.0.alpha.5 - 2026-08-28

- Record the exact local or BoringCache build result for deployment tools.
- Finalize Rails runtime metadata without starting the application command.
- Publish each release with the matching human changelog notes.

## 0.1.0.alpha.4 - 2026-08-28

- Expose the Rails build task as `boring:build`.
- Run the Rails task through the same pretty, streaming CLI experience as
  `boringbuilder build`.
- Keep BoringCache Artifact receipt JSON out of streamed build logs.

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
