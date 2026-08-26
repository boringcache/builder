# Caching

BoringBuilder has two cache paths. The build command and pipeline stay the same for both.

## Local cache

Without BoringCache credentials, Dagger stores its content-addressed build graph plus BoringBuilder's named cache
mounts in the selected engine. Repeated builds on a persistent developer or CI engine reuse them automatically.

This cache belongs to that Dagger engine. A fresh ephemeral runner starts cold unless it connects to a persistent
Dagger engine or uses the shared mode below.

## Shared cache with BoringCache

Shared cache turns on when BoringBuilder finds both:

- `BORINGCACHE_RESTORE_TOKEN` or `BORINGCACHE_SAVE_TOKEN`; and
- `BORINGCACHE_DEFAULT_WORKSPACE`, `BORINGCACHE_WORKSPACE`, or a project `.boringcache.toml` with its workspace.

The built-in Ruby pipeline then runs both `mise install` and `bundle install` through `boringcache run`. Tokens are
Dagger secrets and are not written to the resulting image or artifact. A restore token without a save token selects
read-only mode.

BoringCache persists those directories across fresh engines and runners. When shared caching is enabled it owns the
declared cache directories for that step; Dagger still reuses the rest of its local build graph. Selecting one owner
avoids overlapping mount and restore semantics. Restore, command execution, and save all happen inside the Dagger
container, with no host staging copy.

## Custom application steps

Custom application recipes use the same cache primitive as the built-in Mise and Bundler steps:

```ruby
app = pipeline.run(
  app,
  %w[bundle exec rake compile],
  cache: {
    "compile" => "/app/tmp/cache",
    "downloads" => "/app/tmp/downloads"
  },
  name: "Compile application"
)
```

Each hash key names a stable cache and each value is the directory visible to the command. With BoringCache
credentials those names and paths are restored and saved remotely. Without credentials, Dagger mounts named local
volumes at the same paths. Manual cache paths are removed after the command so dependency stores do not leak into
an image or artifact; copy any required build output elsewhere inside the cached command. Pass the single-cache
shorthand with `entry: "bundler"` (or another built-in/project entry) when BoringCache should use that existing
entry definition.

Cache persistence stays inside the Dagger step. Wrapping `boringbuilder build` with a host-side Docker or cache
command cannot see Dagger's named cache volumes, so BoringBuilder intentionally has no Dockerfile frontend.
