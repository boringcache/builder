# frozen_string_literal: true

require "test_helper"

class ProjectTest < Minitest::Test
  include ProjectFixture

  def test_detects_a_locked_rails_application
    root = build_project(name: "Hello Rails")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    project.validate!

    assert_predicate project, :rails?
    assert_equal "hello-rails", project.app_name
    assert_equal "3.4.9", project.ruby_version
    assert_predicate project, :assets?
    assert_predicate project, :bootsnap?
    assert_equal %w[/rails /usr/local /mise], project.artifact_paths
    assert_equal [:local], project.plan.fetch(:exporters)
  end

  def test_ignores_a_conventional_project_dockerfile
    root = build_project
    File.write(root.join("Dockerfile"), "FROM ruby:3.4-slim\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    project.validate!

    assert_equal "rails", project.plan.fetch(:strategy)
    assert_equal %w[/rails /usr/local /mise], project.artifact_paths
  end

  def test_keeps_environment_files_out_of_the_build_context
    root = build_project
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))
    client = RecordingClient.new

    project.source(client)

    assert_includes BoringBuilder::Project::DEFAULT_EXCLUDES, ".env"
    assert_includes BoringBuilder::Project::DEFAULT_EXCLUDES, ".env.*"
    assert_includes client.calls,
                    [:directory, [root.to_s], { exclude: BoringBuilder::Project::DEFAULT_EXCLUDES, gitignore: true }]
  end

  def test_a_dockerfile_is_not_a_build_definition
    root = build_project
    FileUtils.rm(root.join("Gemfile"))
    FileUtils.rm(root.join("Gemfile.lock"))
    File.write(root.join("Dockerfile"), "FROM alpine:3.22\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    error = assert_raises(BoringBuilder::ConfigurationError) { project.validate! }

    assert_match(/No conventional Ruby application or custom pipeline/, error.message)
  end

  def test_reads_the_ruby_version_from_mise
    root = build_project
    File.write(root.join(".ruby-version"), "3.3.0\n")
    File.write(root.join("mise.toml"), <<~TOML)
      [env]
      ruby = "not-a-tool"

      [tools]
      ruby = "ruby-3.4.10" # production toolchain
    TOML
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    assert_equal "3.4.10", project.ruby_version
  end

  def test_reads_the_ruby_version_from_mise_tool_versions
    root = build_project
    File.write(root.join(".tool-versions"), "node 24.0.0\nruby 3.4.11\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    assert_equal "3.4.11", project.ruby_version
  end

  def test_reads_the_ruby_version_from_dot_mise
    root = build_project
    File.write(root.join(".mise.toml"), <<~TOML)
      [tools]
      ruby = "3.4.12"
    TOML
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    assert_equal "3.4.12", project.ruby_version
  end

  def test_detects_a_rack_application_as_ruby
    root = build_project
    FileUtils.rm(root.join("config/application.rb"))
    File.write(root.join("config.ru"), "run ->(_env) { [200, {}, ['ok']] }\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    project.validate!

    assert_equal :rack, project.framework
    assert_equal "/app", project.application_path
    assert_equal "app", project.application_user
    assert_equal %w[/app /usr/local /mise], project.artifact_paths
    assert_equal %w[bundle exec rackup config.ru --host 0.0.0.0 --port 3000], project.runtime_command
  end

  def test_detects_a_hanami_application_and_its_production_command
    root = build_project
    FileUtils.rm(root.join("config/application.rb"))
    File.write(root.join("config/app.rb"), "class App < Hanami::App; end\n")
    File.write(root.join("config/puma.rb"), "port ENV.fetch('PORT', 3000)\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    project.validate!

    assert_equal :hanami, project.framework
    assert_equal "production", project.runtime_environment.fetch("HANAMI_ENV")
    assert_equal "3000", project.runtime_environment.fetch("HANAMI_PORT")
    assert_equal %w[bundle exec puma -C config/puma.rb], project.runtime_command
  end

  def test_procfile_supplies_a_generic_ruby_start_command
    root = build_project
    FileUtils.rm(root.join("config/application.rb"))
    File.write(root.join("Procfile"), "worker: bundle exec rake jobs:work\nweb: bundle exec falcon serve\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    assert_equal ["/bin/sh", "-lc", "bundle exec falcon serve"], project.runtime_command
    assert_predicate project, :web?
  end

  def test_a_custom_pipeline_can_build_without_a_gemfile
    root = build_project
    FileUtils.rm(root.join("Gemfile"))
    FileUtils.rm(root.join("Gemfile.lock"))
    configuration = BoringBuilder::Configuration.new(root: root)
    configuration.pipeline { |pipeline| pipeline.container("ruby:3.4-slim") }
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new

    project.container(client)

    assert_predicate project, :custom_pipeline?
    assert_equal "custom", project.plan.fetch(:strategy)
    assert_equal ["/"], project.artifact_paths
    assert_includes client.calls, [:from, ["ruby:3.4-slim"], {}]
  end

  def test_builds_a_remapped_custom_artifact
    root = build_project
    release_metadata = root.parent.join("release-metadata")
    FileUtils.mkdir_p(release_metadata)
    File.write(release_metadata.join("revision.txt"), "abc123\n")
    configuration = BoringBuilder::Configuration.new(
      root: root,
      paths: ["/rails=/opt/my-app/current"],
      files: ["/usr/local/bin/mise=/usr/local/bin/mise"],
      host_paths: ["#{release_metadata}=/opt/my-app/release-metadata"]
    )
    project = BoringBuilder::Project.new(configuration).validate!

    entries = project.artifact.to_a

    assert_includes entries,
                    { origin: :container, kind: :directory, source: "/rails",
                      destination: "/opt/my-app/current" }
    assert_includes entries,
                    { origin: :container, kind: :file, source: "/usr/local/bin/mise",
                      destination: "/usr/local/bin/mise" }
    assert_includes entries,
                    { origin: :host, kind: :directory, source: release_metadata.to_s,
                      destination: "/opt/my-app/release-metadata" }
  end

  def test_requires_a_lockfile_for_rails
    root = build_project
    FileUtils.rm(root.join("Gemfile.lock"))
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    error = assert_raises(BoringBuilder::ConfigurationError) { project.validate! }

    assert_match(/Gemfile.lock is required/, error.message)
  end

  def test_requires_a_custom_pipeline_for_native_javascript
    root = build_project
    File.write(root.join("package.json"), "{}\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root))

    error = assert_raises(BoringBuilder::ConfigurationError) { project.validate! }

    assert_match(/JavaScript runtime/, error.message)
  end
end
