# frozen_string_literal: true

require "test_helper"

class PipelineTest < Minitest::Test
  include ProjectFixture

  def test_creates_a_source_mounted_mise_toolchain
    root = build_project
    File.write(root.join("mise.toml"), "[tools]\nnode = \"24\"\n")
    pipeline, client = pipeline_for(environment: {}, root: root)

    pipeline.mise(tools: { node: "24" }, workdir: "/workspace")

    assert_includes client.calls, [:from, [BoringBuilder::Mise::DEFAULT_IMAGE], {}]
    assert_called_with_path client, :with_file, "/workspace/mise.toml"
    assert_called_with_path client, :with_directory, "/workspace"
    BoringBuilder::Mise::ENVIRONMENT.each do |name, value|
      assert_includes client.calls, [:with_env_variable, [name, value], {}]
    end
    config = client.calls.find do |method, arguments, _options|
      method == :with_new_file && arguments.first == "/etc/mise/config.toml"
    end

    assert_includes config[1].fetch(1), '"node" = "24"'
    assert_includes config[1].fetch(1), "paranoid = true"
    assert_includes client.calls, [:with_exec, [BoringBuilder::Mise::INSTALL_COMMAND], {}]
    assert_called_with_path client, :with_mounted_cache, BoringBuilder::Mise::CACHE_PATH
  end

  def test_uses_boringcache_for_the_mise_toolchain_when_shared_credentials_are_present
    pipeline, client = pipeline_for(
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_RESTORE_TOKEN" => "restore-token"
      }
    )

    pipeline.mise(tools: { node: "24" }, workdir: "/workspace")

    assert_includes client.calls,
                    [:with_exec,
                     [%w[boringcache run --entry mise --no-git --read-only -- mise install]], {}]
    refute(client.calls.any? { |method, _arguments, _options| method == :with_mounted_cache })
  end

  def test_rejects_an_invalid_mise_tool_map
    pipeline, = pipeline_for(environment: {})

    error = assert_raises(BoringBuilder::ConfigurationError) do
      pipeline.mise(tools: ["node"], workdir: "/workspace")
    end

    assert_equal "Mise tools must be a name-to-version map", error.message
  end

  def test_runs_a_step_with_a_local_dagger_cache_mount
    pipeline, client = pipeline_for(environment: {})

    pipeline.run(client.container, %w[bundle install], cache: "gems", at: "/bundle/cache")

    assert_includes client.calls, [:cache_volume, ["boringbuilder-my-app-gems"], {}]
    assert(client.calls.any? do |method, arguments, options|
      method == :with_mounted_cache && arguments.first == "/bundle/cache" &&
        options[:sharing] == :LOCKED
    end)
    assert_includes client.calls, [:with_exec, [%w[bundle install]], {}]
    assert_includes client.calls,
                    [:chain_operation, ["withoutMount", { "path" => "/bundle/cache" }], {}]
    assert_includes client.calls, [:with_exec, [["rm", "-rf", "/bundle/cache"]], {}]
  end

  def test_executes_a_named_step_and_prints_its_real_output
    root = build_project
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new(stdout: "compiled\n", stderr: "one warning\n")
    output = StringIO.new
    progress = DaggerRuby::Progress.new(out: output)
    pipeline = BoringBuilder::Pipeline.new(project, client, environment: {}, progress: progress)

    pipeline.exec(client.container, %w[rake compile], name: "Build application")

    assert_includes client.calls, [:stdout, [], {}]
    assert_includes client.calls, [:stderr, [], {}]
    assert_includes client.calls, [:sync, [], {}]
    assert_includes output.string, "#1 Build application\n"
    assert_includes output.string, "    compiled\n    one warning\n"
    assert_match(/#1 DONE \d+\.\d+s\n/, output.string)
  end

  def test_adds_shared_boringcache_persistence_to_the_same_mount
    pipeline, client = pipeline_for(
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_SAVE_TOKEN" => "save-token"
      }
    )

    pipeline.run(client.container, %w[rake compile], cache: "compiled-assets", at: "/app/tmp/cache")

    refute(client.calls.any? { |method, _arguments, _options| method == :with_mounted_cache })
    assert_includes client.calls,
                    [:with_exec,
                     [["boringcache", "run", "--manual-entry",
                       "compiled-assets:/app/tmp/cache",
                       "--no-git", "--", "rake", "compile"]], {}]
    assert_includes client.calls, [:with_exec, [["rm", "-rf", "/app/tmp/cache"]], {}]
    assert(client.calls.any? do |method, arguments, _options|
      method == :with_secret_variable && arguments.first == "BORINGCACHE_SAVE_TOKEN"
    end)
  end

  def test_runs_one_command_with_multiple_local_and_shared_cache_mounts
    pipeline, client = pipeline_for(
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_SAVE_TOKEN" => "save-token"
      }
    )

    pipeline.run(
      client.container,
      %w[cargo build --release],
      cache: {
        "cargo-registry" => "/usr/local/cargo/registry",
        "cargo-target" => "/app/target"
      }
    )

    refute(client.calls.any? { |method, _arguments, _options| method == :cache_volume })
    assert_includes client.calls,
                    [:with_exec,
                     [["boringcache", "run",
                       "--manual-entry", "cargo-registry:/usr/local/cargo/registry",
                       "--manual-entry", "cargo-target:/app/target",
                       "--no-git", "--", "cargo", "build", "--release"]], {}]
  end

  def test_can_use_a_built_in_or_project_boringcache_entry
    pipeline, client = pipeline_for(
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_RESTORE_TOKEN" => "restore-token"
      }
    )

    pipeline.run(client.container, %w[bundle install], cache: "bundle", at: "/bundle/cache", entry: "bundler")

    assert_includes client.calls,
                    [:with_exec,
                     [%w[boringcache run --entry bundler --no-git --read-only -- bundle install]], {}]
  end

  def test_places_project_cache_configuration_in_the_step_workdir
    root = build_project
    File.write(root.join(".boringcache.toml"), "workspace = \"acme/app\"\n")
    pipeline, client = pipeline_for(root: root, environment: { "BORINGCACHE_SAVE_TOKEN" => "save-token" })

    pipeline.run(client.container, %w[rake compile], cache: "compile", at: "/cache", workdir: "/workspace")

    assert(client.calls.any? do |method, arguments, _options|
      method == :with_file && arguments.first == "/workspace/.boringcache.toml"
    end)
  end

  def test_rejects_an_invalid_cache_mount
    pipeline, client = pipeline_for(environment: {})

    error = assert_raises(BoringBuilder::ConfigurationError) do
      pipeline.run(client.container, %w[rake compile], cache: "bad:name", at: "tmp/cache")
    end

    assert_match(/cache name/, error.message)
  end

  private

  def pipeline_for(environment:, root: build_project)
    configuration = BoringBuilder::Configuration.new(root: root)
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    [BoringBuilder::Pipeline.new(project, client, environment: environment), client]
  end

  def assert_called_with_path(client, expected_method, expected_path)
    called = client.calls.any? do |method, arguments, _options|
      method == expected_method && arguments.first == expected_path
    end

    assert called, "Expected #{expected_method} with #{expected_path}"
  end
end
