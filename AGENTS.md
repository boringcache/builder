# Working on BoringBuilder

Read [SOUL.md](SOUL.md) and [STYLE.md](STYLE.md) before changing the product, code, or public language.

## Product shape

- BoringBuilder builds any application. Rails is the first-class convention over a shared Mise-powered foundation.
- Hanami, Rack, and other Bundler applications share the built-in Ruby foundation.
- Other workloads use the same Mise foundation from an ordinary Ruby recipe backed by Dagger. Ruby is the extension
  language, not an application requirement.
- Keep `boringbuilder build` conventional and `boringbuilder init` the small escape hatch.
- Do not introduce a Dockerfile frontend, a build YAML language, or provider-specific behavior into the build graph.
- Local and BoringCache caches use the same declared step paths. Local and BoringCache artifact exporters receive the
  same normalized artifact.

## Engineering habits

- Write clear Ruby against the current development toolchain while preserving the support contract in the gemspec
  and CI.
- Keep the public surface small. Prefer a plain object or method over a new abstraction.
- Treat commands, paths, build contexts, archives, registries, and credentials as security boundaries.
- Never commit credentials, private source, local absolute paths, generated gems, dependency directories, or
  application-specific integration details.
- Keep documentation human, direct, and useful to someone building their first production artifact.
- Add focused tests for behavior changes. Run `bin/ci` before committing.
- Run the relevant live Dagger build when changing the build, cache, runtime, or export graph.
- Inspect the built gem and the complete diff before release work.
- Treat these as defaults with reasons, not substitutes for repository invariants, security boundaries, or evidence.

## Product checks

Before handing off a change, ask:

1. Does the conventional Rails path remain simple?
2. Can a non-Ruby application still express the same idea with a small recipe?
3. Are local and shared caching behavior understandable?
4. Can the result be exported or published without rebuilding it differently?
5. Is the code pleasant for the next human—or agent—to read?
