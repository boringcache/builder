# frozen_string_literal: true

require "test_helper"

class RailsBuildTest < Minitest::Test
  include ProjectFixture

  def test_builds_a_production_rails_container_with_oci_metadata
    root = build_project
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RailsBuild.new(project, client).container

    methods = client.calls.map(&:first)

    assert_includes methods, :cache_volume
    assert_includes methods, :with_mounted_cache
    assert_includes client.calls, [:with_exec, [BoringBuilder::RubyBuild::BUNDLE_INSTALL_COMMAND], {}]
    assert_includes client.calls, [:with_exec, [%w[bin/rails assets:precompile]], {}]
    assert_includes client.calls, [:with_exec, [%w[bundle exec bootsnap precompile --gemfile app/ lib/]], {}]
    assert_includes client.calls,
                    [:with_exec, [%w[rm -f tmp/local_secret.txt tmp/cache/bootsnap/load-path-cache]], {}]
    assert_includes client.calls, [:with_default_args, [%w[server]], {}]
    assert_includes methods, :with_docker_healthcheck
    assert_includes client.calls, [:with_user, ["rails"], {}]
  end

  def test_keeps_build_packages_out_of_the_runtime_stage
    root = build_project
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RailsBuild.new(project, client).container

    owned_directories = client.calls.filter_map do |method, arguments, options|
      arguments.first if method == :with_directory && options[:owner] == "rails:rails"
    end

    assert_equal 2, client.calls.count([:container, [{}], {}])
    assert_includes client.calls,
                    [:with_exec, [["apt-get", "install", "-y", "--no-install-recommends",
                                   "build-essential", "curl", "git", "libpq-dev", "libyaml-dev", "pkg-config"]], {}]
    assert_includes client.calls,
                    [:with_exec, [["apt-get", "install", "-y", "--no-install-recommends", "curl", "libpq5"]], {}]
    assert_equal ["/rails", "/rails", "/usr/local/bundle"], owned_directories.sort
  end

  def test_uses_rails_docker_entrypoint_when_present
    root = build_project
    File.write(root.join("bin/docker-entrypoint"), "#!/bin/sh\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RailsBuild.new(project, client).container

    assert_includes client.calls, [:with_entrypoint, [%w[/rails/bin/docker-entrypoint]], {}]
    assert_includes client.calls, [:with_default_args, [%w[./bin/rails server]], {}]
  end

  def test_uses_thruster_when_present
    root = build_project
    File.write(root.join("bin/docker-entrypoint"), "#!/bin/sh\n")
    File.write(root.join("bin/thrust"), "#!/usr/bin/env ruby\n")
    project = BoringBuilder::Project.new(BoringBuilder::Configuration.new(root: root)).validate!
    client = RecordingClient.new

    BoringBuilder::RailsBuild.new(project, client).container

    assert_equal 80, project.runtime_port
    assert_equal "80", project.runtime_environment.fetch("PORT")
    assert_includes client.calls, [:with_entrypoint, [%w[/rails/bin/docker-entrypoint]], {}]
    assert_includes client.calls, [:with_default_args, [%w[./bin/thrust ./bin/rails server]], {}]
    assert(client.calls.any? do |method, arguments, _options|
      method == :with_exposed_port && arguments.first == 80
    end)
    assert(client.calls.any? do |method, arguments, _options|
      method == :with_docker_healthcheck && arguments.first.any? { |argument| argument.include?("localhost:80/up") }
    end)
  end
end
