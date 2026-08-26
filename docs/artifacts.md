# Artifacts and images

BoringBuilder exports a directory, tar archive, reproducible zstd tar archive, OCI image archive, or Docker image
archive. It can also publish artifacts to BoringCache, load an image into the selected runtime, or publish an image
to a registry.

## Export destinations

The default exporter is automatic:

- with a BoringCache save token and workspace, publish a shared BoringCache Artifact;
- otherwise, write the artifact under `dist/`;
- when `--output` is explicit, always keep that local copy as well.

Override the destination when needed:

```sh
bundle exec boringbuilder build --exporter local
bundle exec boringbuilder build --exporter boringcache --artifact-name web-production
bundle exec boringbuilder build --json
```

The BoringCache exporter uploads directly from the Dagger graph and returns a ready Artifact ID. It does not stage
the build through a host directory. Exporters are small Ruby adapters around the same normalized build asset, so a
future repository backend does not change the build pipeline or artifact model.

## Filesystem artifacts

The default format is `tar.zst`. The built-in Ruby pipeline selects the application (`/rails` for Rails or `/app`
for other Ruby applications), `/usr/local`, and `/mise`. A custom pipeline selects the complete container root
unless the application narrows it.

```sh
bundle exec boringbuilder build --format directory --output dist/rootfs
bundle exec boringbuilder build --format tar --output dist/app.tar
bundle exec boringbuilder build --format tar.zst --output dist/app.tar.zst
```

A directory export replaces its destination. Archive exports replace the destination file.

Tar exports normalize ordering, timestamps, and ownership so an unchanged build produces the same archive.

Select or remap container paths with repeatable options:

```sh
bundle exec boringbuilder build \
  --path /rails=/opt/my-app/current \
  --path /usr/local/bundle=/opt/my-app/current/vendor/bundle \
  --file /usr/local/bin/mise=/usr/local/bin/mise \
  --host-path ../release-metadata=/opt/my-app/release-metadata \
  --format tar.zst \
  --output dist/my-app-snapshot.tar.zst
```

`--host-path` is explicit and requires a destination. It adds release metadata without widening the Docker build
context.

For a reusable layout, put the same choices in `config/boringbuilder.rb`:

```ruby
BoringBuilder.configure do |config|
  config.artifact.directory("/rails", at: "/opt/my-app/current")
  config.artifact.directory("/usr/local/bundle", at: "/opt/my-app/current/vendor/bundle")
  config.artifact.file("/usr/local/bin/mise")
  config.artifact.host_path("../release-metadata", at: "/opt/my-app/release-metadata")
end
```

## Container images

Create portable image archives:

```sh
bundle exec boringbuilder build --format oci --output dist/app.oci.tar
bundle exec boringbuilder build --format docker --output dist/app.docker.tar
```

Load or publish the same built image:

```sh
bundle exec boringbuilder build --load my-app:latest
bundle exec boringbuilder build --push ghcr.io/acme/my-app:latest
```

Pass `--output` as well when both a local archive and a loaded or published image are required. Registry credentials
are owned by Dagger and the selected runtime; BoringBuilder does not accept them as command-line values.

## Ruby API

Applications and deployment tools can use the same configuration directly:

```ruby
result = BoringBuilder.build(
  root: Rails.root,
  runtime: :docker,
  platform: "linux/arm64",
  format: :tar_zst,
  output: Rails.root.join("dist/app.tar.zst")
) do |config|
  config.artifact.directory("/rails", at: "/opt/my-app/current")
  config.artifact.directory("/usr/local/bundle", at: "/opt/my-app/current/vendor/bundle")
end

puts result.path
puts result.artifact_id
```
