# frozen_string_literal: true

require "test_helper"

class RuntimeTest < Minitest::Test
  def test_prefers_apple_container_on_macos
    probed = []
    probe = lambda do |runtime, _command|
      probed << runtime
      BoringBuilder::Runtime::Probe.new(success: runtime == :apple, message: "not ready")
    end

    runtime = BoringBuilder::Runtime.new(:auto, probe: probe, host_os: "arm64-darwin")

    assert_equal :apple, runtime.resolve
    assert_equal [:apple], probed
  end

  def test_prefers_docker_on_linux
    probe = lambda do |runtime, _command|
      BoringBuilder::Runtime::Probe.new(success: runtime == :docker, message: "not ready")
    end

    runtime = BoringBuilder::Runtime.new(:auto, probe: probe, host_os: "x86_64-linux")

    assert_equal :docker, runtime.resolve
  end

  def test_reports_all_auto_detection_failures
    probe = lambda do |runtime, _command|
      BoringBuilder::Runtime::Probe.new(success: false, message: "#{runtime} unavailable")
    end

    error = assert_raises(BoringBuilder::RuntimeError) do
      BoringBuilder::Runtime.new(:auto, probe: probe, host_os: "arm64-darwin").resolve
    end

    assert_match(/apple: apple unavailable/, error.message)
    assert_match(/docker: docker unavailable/, error.message)
  end

  def test_explicit_runtime_does_not_fall_back
    probe = lambda do |_runtime, _command|
      BoringBuilder::Runtime::Probe.new(success: false, message: "daemon stopped")
    end

    error = assert_raises(BoringBuilder::RuntimeError) do
      BoringBuilder::Runtime.new(:docker, probe: probe).resolve
    end

    assert_match(/Docker runtime is not ready/, error.message)
  end

  def test_preserves_the_apple_runner_inside_a_dagger_session
    with_environment(
      "DAGGER_SESSION_PORT" => "1234",
      "DAGGER_SESSION_TOKEN" => "token",
      "_EXPERIMENTAL_DAGGER_RUNNER_HOST" =>
        "image+apple://registry.dagger.io/engine:v#{DaggerRuby::DAGGER_VERSION}"
    ) do
      assert_equal :apple, BoringBuilder::Runtime.new(:auto).resolve
    end
  end

  def test_uses_a_configured_linux_vm_runner_without_probing_the_host
    probed = []
    probe = lambda do |runtime, _command|
      probed << runtime
      BoringBuilder::Runtime::Probe.new(success: false, message: "not ready")
    end

    with_environment("_EXPERIMENTAL_DAGGER_RUNNER_HOST" => "tcp://builder.internal:1234") do
      runtime = BoringBuilder::Runtime.new(:auto, probe: probe)

      assert_equal :custom, runtime.resolve
      assert_equal :auto, runtime.dagger_runtime(:custom)
      assert_empty probed
    end
  end

  private

  def with_environment(values)
    original = values.to_h { |name, _value| [name, ENV.fetch(name, nil)] }
    values.each { |name, value| ENV[name] = value }
    yield
  ensure
    original.each { |name, value| ENV[name] = value }
  end
end
