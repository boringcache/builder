# frozen_string_literal: true

require "test_helper"

class BoringCacheTest < Minitest::Test
  include ProjectFixture

  def test_defaults_to_the_local_dagger_cache
    project = project_for(build_project)
    cache = BoringBuilder::BoringCache.new(project, RecordingClient.new, environment: {})

    refute_predicate cache, :enabled?
    assert_equal :local, cache.mode
    assert_equal %w[bundle install], cache.wrap(%w[bundle install])
  end

  def test_wraps_bundler_inside_the_engine_when_credentials_are_present
    project = project_for(build_project)
    client = RecordingClient.new
    cache = BoringBuilder::BoringCache.new(
      project,
      client,
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_RESTORE_TOKEN" => "restore-token",
        "BORINGCACHE_SAVE_TOKEN" => "save-token"
      }
    )

    container = cache.prepare(client.container)
    command = cache.wrap(%w[bundle install])

    assert_predicate cache, :enabled?
    assert_equal :boringcache, cache.mode
    assert_equal %w[boringcache run --entry bundler --no-git -- bundle install], command
    assert_same container, client.container
    assert(client.calls.any? do |name, arguments, _|
      name == :with_file && arguments.first == "/usr/local/bin/boringcache"
    end)
    assert(client.calls.any? do |name, arguments, _|
      name == :with_secret_variable && arguments.first == "BORINGCACHE_RESTORE_TOKEN"
    end)
    assert(client.calls.any? do |name, arguments, _|
      name == :with_env_variable && arguments == ["BORINGCACHE_DEFAULT_WORKSPACE", "acme/app"]
    end)
  end

  def test_restore_only_credentials_select_read_only_mode
    project = project_for(build_project)
    cache = BoringBuilder::BoringCache.new(
      project,
      RecordingClient.new,
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_RESTORE_TOKEN" => "restore-token"
      }
    )

    assert_equal %w[boringcache run --entry bundler --no-git --read-only -- bundle install],
                 cache.wrap(%w[bundle install])
    refute_predicate cache, :artifact_publishable?
  end

  def test_wraps_the_mise_entry_with_the_same_cache_lifecycle
    project = project_for(build_project)
    cache = BoringBuilder::BoringCache.new(
      project,
      RecordingClient.new,
      environment: {
        "BORINGCACHE_DEFAULT_WORKSPACE" => "acme/app",
        "BORINGCACHE_SAVE_TOKEN" => "save-token"
      }
    )

    assert_equal %w[boringcache run --entry mise --no-git -- mise install],
                 cache.wrap(%w[mise install], entry: "mise")
    assert_predicate cache, :artifact_publishable?
  end

  private

  def project_for(root)
    configuration = BoringBuilder::Configuration.new(root: root)
    BoringBuilder::Project.new(configuration).validate!
  end
end
