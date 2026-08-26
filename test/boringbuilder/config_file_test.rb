# frozen_string_literal: true

require "test_helper"

class ConfigFileTest < Minitest::Test
  include ProjectFixture

  def test_loads_plain_ruby_configuration
    root = build_project
    File.write(root.join("config/boringbuilder.rb"), <<~RUBY)
      BoringBuilder.configure do |config|
        config.runtime = :apple
        config.format = :tar_zst
        config.output = "tmp/builds/app.tar.zst"
        config.artifact.directory("/rails", at: "/opt/app/current")
      end
    RUBY

    configuration = BoringBuilder.configuration(root: root)
    project = BoringBuilder::Project.new(configuration).validate!

    assert_equal :apple, configuration.runtime
    assert_equal :tar_zst, configuration.format
    assert_equal root.join("tmp/builds/app.tar.zst"), configuration.output
    assert_equal "/opt/app/current", project.artifact.entries.first.destination
  end

  def test_explicit_options_override_the_config_file
    root = build_project
    File.write(root.join("config/boringbuilder.rb"), <<~RUBY)
      BoringBuilder.configure do |config|
        config.runtime = :apple
        config.format = :tar_zst
      end
    RUBY

    configuration = BoringBuilder.configuration(root: root, config: :auto, runtime: "docker", format: "oci")
    configuration.validate!

    assert_equal :docker, configuration.runtime
    assert_equal :oci, configuration.format
  end

  def test_recipe_can_define_a_custom_dagger_pipeline
    root = build_project
    File.write(root.join("config/boringbuilder.rb"), <<~RUBY)
      BoringBuilder.configure do |config|
        config.pipeline do |pipeline|
          pipeline.container("ruby:4.0-slim")
        end
      end
    RUBY

    configuration = BoringBuilder.configuration(root: root)
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    project.container(client)

    assert_equal "custom", project.plan.fetch(:strategy)
    assert_includes client.calls, [:from, ["ruby:4.0-slim"], {}]
  end

  def test_rejects_a_config_file_without_a_recipe
    root = build_project
    File.write(root.join("config/boringbuilder.rb"), "SOME_CONSTANT = true\n")

    error = assert_raises(BoringBuilder::ConfigurationError) do
      BoringBuilder.configuration(root: root, config: :auto)
    end

    assert_match(/must call BoringBuilder\.configure/, error.message)
  end
end
