# frozen_string_literal: true

require "test_helper"

class ConfigurationTest < Minitest::Test
  include ProjectFixture

  def test_normalizes_runtime_format_and_output
    root = build_project
    configuration = BoringBuilder::Configuration.new(
      root: root,
      runtime: "apple",
      format: "tar.zst",
      output: "artifacts/app.tar.zst"
    )

    assert_equal :apple, configuration.runtime
    assert_equal :tar_zst, configuration.format
    assert_equal root.join("artifacts/app.tar.zst"), configuration.output
  end

  def test_rejects_unknown_runtime
    error = assert_raises(BoringBuilder::ConfigurationError) do
      BoringBuilder::Configuration.new(runtime: "podman")
    end

    assert_match(/auto, apple, or docker/, error.message)
  end

  def test_rejects_unsafe_artifact_paths
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, paths: ["../etc"])

    error = assert_raises(BoringBuilder::ConfigurationError) do
      BoringBuilder::Project.new(configuration).validate!.artifact
    end

    assert_match(/clean absolute path/, error.message)
  end

  def test_rejects_an_unknown_artifact_object
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, artifact: Object.new)

    error = assert_raises(BoringBuilder::ConfigurationError) { configuration.validate! }

    assert_match(/BoringBuilder::Artifact/, error.message)
  end

  def test_rejects_push_and_load_together
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, publish: "example/app", load: "app:latest")

    assert_raises(BoringBuilder::ConfigurationError) { configuration.validate! }
  end

  def test_rejects_a_directory_export_over_the_project
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, format: :directory, output: root)

    error = assert_raises(BoringBuilder::ConfigurationError) { configuration.validate! }

    assert_match(/unsafe output path/, error.message)
  end

  def test_rejects_a_non_positive_port
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, port: 0)

    error = assert_raises(BoringBuilder::ConfigurationError) { configuration.validate! }

    assert_match(/positive integer/, error.message)
  end

  def test_rejects_a_non_numeric_port
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, port: "web")

    error = assert_raises(BoringBuilder::ConfigurationError) { configuration.validate! }

    assert_match(/positive integer/, error.message)
  end

  def test_normalizes_the_exporter
    root = build_project

    automatic = BoringBuilder::Configuration.new(root: root)
    local = BoringBuilder::Configuration.new(root: root, exporter: "local")

    assert_equal :auto, automatic.exporter
    assert_equal :local, local.exporter
  end

  def test_rejects_an_unknown_exporter
    error = assert_raises(BoringBuilder::ConfigurationError) do
      BoringBuilder::Configuration.new(exporter: :s3)
    end

    assert_match(/auto, local, or boringcache/, error.message)
  end

  def test_stores_a_custom_pipeline
    root = build_project
    recipe = proc { |_pipeline| Object.new }
    configuration = BoringBuilder::Configuration.new(root: root)

    configuration.pipeline(&recipe)

    assert_same recipe, configuration.pipeline
  end

  def test_normalizes_additional_system_packages
    configuration = BoringBuilder::Configuration.new(
      root: build_project,
      build_packages: [:libsodium_dev],
      runtime_packages: ["libsodium23"],
      build_environment: { REDIS_URL: "redis://127.0.0.1:6379/0" }
    )

    assert_equal ["libsodium_dev"], configuration.build_packages
    assert_equal ["libsodium23"], configuration.runtime_packages
    assert_equal({ "REDIS_URL" => "redis://127.0.0.1:6379/0" }, configuration.build_environment)
  end

  def test_rejects_a_noncallable_pipeline
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, pipeline: Object.new)

    error = assert_raises(BoringBuilder::ConfigurationError) { configuration.validate! }

    assert_match(/pipeline must be callable/, error.message)
  end
end
