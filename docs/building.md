# Building applications

BoringBuilder resolves one production build for an application, then executes it through Dagger. Run commands from
the application root unless a project directory is passed explicitly.

```sh
bundle exec boringbuilder build
bundle exec boringbuilder build /path/to/application
```

## Build resolution

BoringBuilder chooses one pipeline:

1. the custom Dagger Ruby pipeline in `config/boringbuilder.rb`, when configured;
2. otherwise, the built-in Ruby pipeline.

An application recipe can also tune the built-in pipeline and define its artifact layout. Command-line options
override those settings for a one-off build.

```sh
bundle exec boringbuilder build --config config/release.rb
bundle exec boringbuilder build --no-config
```

Applications with `package.json` must provide a custom pipeline. BoringBuilder does not guess which JavaScript
runtime or package manager an application uses.

## Generate a recipe

Run `init` when an application needs a custom pipeline or checked-in overrides:

```sh
boringbuilder init
```

BoringBuilder detects a conventional Ruby, Node, Rust, or Go project and writes `config/boringbuilder.rb`. An
unrecognized project receives a small generic Dagger recipe. Select a starting point explicitly when detection is
not enough:

```sh
boringbuilder init --template node
boringbuilder init --template rust
boringbuilder init --template go
boringbuilder init --template generic
```

The generated file is ordinary Ruby using BoringBuilder's Mise toolchain, Dagger containers, and cache/export
conventions. It is a starting point owned by the application, not generated state or a separate configuration
language. Recipes execute as Ruby on the host, so review them before building source you do not trust. `init` refuses
to replace an existing recipe; pass `--force` only when replacement is intentional.

## Built-in Rails and Ruby pipeline

The built-in pipeline:

- reads the Ruby version from `mise.toml`, `.mise.toml`, `.tool-versions`, `.ruby-version`, `Gemfile`, or
  `Gemfile.lock`;
- installs the toolchain with Mise and locked production gems with separate persistent caches;
- separates build packages from the runtime image;
- places the application under a non-root user; and
- leaves runtime secrets out of the image and exported artifact.

`mise.toml` (or `.mise.toml`) is read from its `[tools]` section using the conventional exact string form:

```toml
[tools]
ruby = "4.0.6"
```

The project file is copied into the build so its Ruby version and Mise settings are respected. The built-in path
installs Ruby only; custom pipelines can install any additional tools. When no Mise file is present, BoringBuilder
generates the minimal Ruby declaration from the other version sources.

## Framework conventions

A root `Gemfile` selects the Ruby builder. Rails applications are detected by `config/application.rb` and receive
asset, Bootsnap, entrypoint, Thruster, port, and `/up` health-check conventions.

Hanami applications are detected by `config/app.rb` and use the production Puma configuration when present. Rack
applications are detected by `config.ru`. For any Ruby application, a `web:` entry in `Procfile` takes precedence.
Applications without a detected process still build a complete artifact; set a command in the recipe when producing
a directly runnable image:

```ruby
BoringBuilder.configure do |config|
  config.command = %w[bundle exec sidekiq]
  config.port = 3000
end
```

Add application-specific Debian packages without replacing the built-in pipeline:

```ruby
BoringBuilder.configure do |config|
  config.build_packages += %w[libsodium-dev]
  config.runtime_packages += %w[libsodium23]
  config.build_environment["REDIS_URL"] = "redis://127.0.0.1:6379/0"
end
```

`build_environment` is for non-secret values needed while compiling. Use Dagger secrets in a custom pipeline for
sensitive build inputs.

Use a custom pipeline when the application needs JavaScript package installation, unusual system packages, a
different operating-system foundation, or is not a Ruby application.

## Mise-powered application pipeline

For any application outside the built-in conventions, keep the build definition in Ruby. A recipe block receives a
small pipeline object and returns the final Dagger container:

```ruby
# config/boringbuilder.rb
BoringBuilder.configure do |config|
  config.artifact.directory("/app/dist", at: "/app")

  config.pipeline do |pipeline|
    app = pipeline.mise(tools: { node: "24" }, workdir: "/app")

    app = pipeline.run(
      app,
      %w[npm ci],
      cache: "npm-downloads",
      at: "/root/.npm/_cacache",
      workdir: "/app",
      name: "Install dependencies"
    )

    pipeline.exec(app, %w[npm run build], name: "Build application")
  end
end
```

`pipeline.mise` prepares the requested tools and application source in a Dagger container. Project versions in
`mise.toml`, `.mise.toml`, or `.tool-versions` override recipe fallbacks, and `mise.lock` is respected when present.
Any tool in the [Mise registry](https://mise.jdx.dev/registry.html) can use the same shape.

`pipeline.run` persists the declared path in the local Dagger engine or through BoringCache when shared credentials
are present. `pipeline.exec` runs an ordinary named command. Both execute as real Dagger boundaries and show the
command's output beneath their numbered build step. Use a hash to cache several paths in one step. See
[Caching](caching.md) for both modes.

Both helpers return ordinary Dagger Ruby containers and use the same artifact and image exporters as the built-in
build. `pipeline.container` remains available when an application needs a different foundation.

## Container runtime

`auto` prefers Apple Container on supported Apple silicon Macs and Docker elsewhere. A configured Dagger session can
point at an engine on a Linux VM without changing the build. Select a local runtime explicitly in CI and other
reproducible environments:

```sh
bundle exec boringbuilder doctor --runtime apple
bundle exec boringbuilder build --runtime apple
bundle exec boringbuilder build --runtime docker
```

Start Apple Container with `container system start`, or start the Docker daemon before selecting Docker. An existing
Dagger session or custom runner is used without probing the host runtime.

Dagger CLI and Engine versions must match the version required by Dagger Ruby. `boringbuilder doctor` checks that
contract before a build; pointing a different CLI at the expected Engine image is not an equivalent setup.

## Build options

Target a deployment platform or inspect the resolved plan without building:

```sh
bundle exec boringbuilder build --platform linux/arm64
bundle exec boringbuilder build --command "bundle exec puma -C config/puma.rb" --port 3000
bundle exec boringbuilder build --dry-run
```

Run `bundle exec boringbuilder build --help` for the complete command reference.
