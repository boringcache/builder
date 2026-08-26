# frozen_string_literal: true

require "test_helper"

class RubyBuildTest < Minitest::Test
  include ProjectFixture

  def test_installs_ruby_and_gems_with_persisted_caches
    client = build_rack_container

    assert_includes client.calls, [:with_exec, [[*BoringBuilder::Mise::INSTALL_COMMAND, "ruby"]], {}]
    assert_includes client.calls, [:with_exec, [BoringBuilder::RubyBuild::BUNDLE_INSTALL_COMMAND], {}]
    assert_includes client.calls,
                    [:with_exec,
                     [["find", "/usr/local/bundle", "-path", "*/cache/*.gem", "-type", "f", "-delete"]], {}]
    assert_called_with_path client, :with_mounted_cache, "/mise/cache"
    assert_called_with_path client, :with_mounted_cache, "/usr/local/bundle/cache"
    assert_called_with_path client, :with_directory, "/mise/installs"
    assert(client.calls.any? do |method, _arguments, options|
      method == :with_mounted_cache && options[:sharing] == :LOCKED
    end)
    config = client.calls.find do |method, arguments, _options|
      method == :with_new_file && arguments.first == "/etc/mise/config.toml"
    end

    assert_includes config[1].fetch(1), '"ruby" = "3.4.9"'
  end

  def test_installs_only_ruby_from_a_project_mise_file
    root = rack_project
    File.write(root.join("mise.toml"), <<~TOML)
      [tools]
      ruby = "3.4.9"
      gum = "0.17.0"
    TOML
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RubyBuild.new(project, client).container

    assert_includes client.calls, [:with_exec, [%w[mise install ruby]], {}]
    refute_includes client.calls, [:with_exec, [%w[mise install]], {}]
  end

  def test_configures_the_detected_rack_runtime
    client = build_rack_container

    assert_includes client.calls,
                    [:with_default_args,
                     [%w[bundle exec rackup config.ru --host 0.0.0.0 --port 3000]], {}]
    assert_includes client.calls, [:with_user, ["app"], {}]
  end

  def test_writes_the_portable_bundle_configuration_into_the_application
    client = build_rack_container
    call = client.calls.find do |method, arguments, _options|
      method == :with_new_file && arguments.first.end_with?("/.bundle/config")
    end

    refute_nil call
    assert_equal BoringBuilder::RubyBuild::BUNDLE_CONFIG, call[1].fetch(1)
    assert_empty call.last
  end

  def test_copies_the_ruby_version_file_before_bundle_install
    root = rack_project
    File.write(root.join(".ruby-version"), "3.4.9\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RubyBuild.new(project, client).container

    assert_called_with_path client, :with_file, "/app/.ruby-version"
    assert_includes client.calls, [:file, [root.join(".ruby-version").to_s], {}]
  end

  def test_mounts_dependency_files_independently_from_application_source
    root = rack_project
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RubyBuild.new(project, client).container

    assert_includes client.calls, [:file, [root.join("Gemfile").to_s], {}]
    assert_includes client.calls, [:file, [root.join("Gemfile.lock").to_s], {}]
    assert_includes client.calls,
                    [:directory, [root.to_s], { exclude: BoringBuilder::Project::DEFAULT_EXCLUDES, gitignore: true }]
  end

  def test_installs_application_specific_system_packages_in_the_right_stage
    root = rack_project
    configuration = BoringBuilder::Configuration.new(
      root: root,
      build_packages: ["libsodium-dev"],
      runtime_packages: ["libsodium23"],
      build_environment: { "REDIS_URL" => "redis://127.0.0.1:6379/0" }
    )
    client = RecordingClient.new

    BoringBuilder::RubyBuild.new(BoringBuilder::Project.new(configuration).validate!, client).container

    assert(client.calls.any? do |method, arguments, _options|
      method == :with_exec && arguments.first.include?("libsodium-dev")
    end)
    assert(client.calls.any? do |method, arguments, _options|
      method == :with_exec && arguments.first.include?("libsodium23")
    end)
    assert_includes client.calls,
                    [:with_env_variable, ["REDIS_URL", "redis://127.0.0.1:6379/0"], {}]
  end

  private

  def build_rack_container
    root = rack_project
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RubyBuild.new(project, client).container
    client
  end

  def rack_project
    root = build_project
    FileUtils.rm(root.join("config/application.rb"))
    FileUtils.rm_rf(root.join("app/assets"))
    File.write(root.join("config.ru"), "run ->(_env) { [200, {}, ['ok']] }\n")
    root
  end

  def assert_called_with_path(client, expected_method, expected_path)
    called = client.calls.any? do |method, arguments, _|
      method == expected_method && arguments.first == expected_path
    end

    assert called, "Expected #{expected_method} with #{expected_path}"
  end
end
