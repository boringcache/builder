# frozen_string_literal: true

require "test_helper"

class ExportersTest < Minitest::Test
  include ProjectFixture

  def test_resolves_local_without_stage_credentials
    resolver = resolver_for(environment: {})

    assert_equal [:local], resolver.plan
  end

  def test_resolves_boringcache_with_a_save_token_and_workspace
    resolver = resolver_for(environment: shared_environment)

    assert_equal [:boringcache], resolver.plan
  end

  def test_explicit_output_keeps_a_local_copy_beside_the_shared_artifact
    resolver = resolver_for(environment: shared_environment, output: "dist/release.tar.zst")

    assert_equal %i[boringcache local], resolver.plan
  end

  def test_registry_publish_needs_no_filesystem_exporter
    resolver = resolver_for(environment: {}, publish: "registry.example/app:latest")

    assert_empty resolver.plan
  end

  def test_local_directory_export_replaces_the_destination
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, format: :directory)
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    source = RecordingNode.new(client.calls)
    asset = BoringBuilder::Exporters::Asset.new(kind: :directory, source: source, filename: "rootfs")

    receipt = BoringBuilder::Exporters::Local.new(project).export(asset)

    assert_equal project.output_path.to_s, receipt.path
    assert_includes client.calls, [:export, [project.output_path.to_s], { wipe: true }]
  end

  def test_explicit_boringcache_exporter_fails_without_save_credentials
    resolver = resolver_for(exporter: :boringcache, environment: {
                              "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
                              "BORINGCACHE_RESTORE_TOKEN" => "restore-token"
                            })

    error = assert_raises(BoringBuilder::ConfigurationError) { resolver.validate! }

    assert_match(/BORINGCACHE_SAVE_TOKEN/, error.message)
  end

  def test_boringcache_exporter_publishes_and_returns_a_ready_receipt
    receipt = {
      artifact: {
        id: "art_0123456789abcdef01234567",
        name: "my-app-linux-arm64-tar-zst",
        status: "ready"
      }
    }
    client = RecordingClient.new(stdout: JSON.generate(receipt))
    resolver = resolver_for(client: client, platform: "linux/arm64", environment: shared_environment)
    exporter = resolver.destinations.fetch(0)
    asset = BoringBuilder::Exporters::Asset.new(
      kind: :file,
      source: RecordingNode.new(client.calls),
      filename: "app.tar.zst"
    )

    result = exporter.export(asset)

    assert_equal "art_0123456789abcdef01234567", result.artifact_id
    assert_equal "my-app-linux-arm64-tar-zst", result.artifact_name
    assert_includes client.calls, [:set_secret, %w[BORINGCACHE_SAVE_TOKEN save-token], {}]
    assert(client.calls.any? do |method, arguments, _options|
      method == :with_exec && arguments.first == [
        "boringcache", "artifact", "push", "/artifact/app.tar.zst",
        "--name", "my-app-linux-arm64-tar-zst", "--include-hidden", "--json",
        "--compression", "none"
      ]
    end)
  end

  private

  def resolver_for(environment:, client: RecordingClient.new, platform: nil, exporter: :auto,
                   output: nil, publish: nil)
    root = build_project
    configuration = BoringBuilder::Configuration.new(
      root: root,
      platform: platform,
      exporter: exporter,
      output: output,
      publish: publish
    )
    project = BoringBuilder::Project.new(configuration)
    BoringBuilder::Exporters::Resolver.new(project, client, environment: environment)
  end

  def shared_environment
    {
      "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
      "BORINGCACHE_SAVE_TOKEN" => "save-token"
    }
  end
end
