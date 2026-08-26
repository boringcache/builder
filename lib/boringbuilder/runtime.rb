# frozen_string_literal: true

require "open3"
require "timeout"

module BoringBuilder
  class Runtime
    Probe = Data.define(:success, :message) do
      alias_method :success?, :success
    end
    RUNTIME_COMMANDS = {
      apple: %w[container system status],
      docker: %w[docker version]
    }.freeze

    attr_reader :requested

    def initialize(requested, probe: nil, host_os: RUBY_PLATFORM)
      @requested = requested.to_sym
      @probe = probe || method(:probe_command)
      @host_os = host_os
    end

    def resolve
      return session_runtime if dagger_session?
      return :custom if custom_runner?
      return ensure_available!(requested) unless requested == :auto

      failures = []
      runtime_order.each do |runtime|
        result = @probe.call(runtime, RUNTIME_COMMANDS.fetch(runtime))
        return runtime if result.success?

        failures << "#{runtime}: #{result.message}"
      end

      raise BoringBuilder::RuntimeError, <<~MESSAGE.strip
        No supported container runtime is ready. Start Apple Container with `container system start`
        or start Docker, then try again. Probes: #{failures.join('; ')}
      MESSAGE
    end

    def dagger_runtime(resolved = resolve)
      %i[session custom].include?(resolved) ? :auto : resolved
    end

    private

    def ensure_available!(runtime)
      result = @probe.call(runtime, RUNTIME_COMMANDS.fetch(runtime))
      return runtime if result.success?

      hint = runtime == :apple ? "container system start" : "start the Docker daemon"
      raise BoringBuilder::RuntimeError,
            "#{runtime.to_s.capitalize} runtime is not ready: #{result.message}. Try `#{hint}`."
    end

    def runtime_order
      @host_os.include?("darwin") ? %i[apple docker] : %i[docker apple]
    end

    def dagger_session?
      ENV.fetch("DAGGER_SESSION_PORT", nil) && ENV.fetch("DAGGER_SESSION_TOKEN", nil)
    end

    def custom_runner?
      ENV.fetch("_EXPERIMENTAL_DAGGER_RUNNER_HOST", nil) && requested == :auto
    end

    def session_runtime
      runner_host = ENV.fetch("_EXPERIMENTAL_DAGGER_RUNNER_HOST", nil)
      match = runner_host&.match(%r{\Aimage\+(apple|docker)://})
      return match[1].to_sym if match
      return :custom if runner_host

      :session
    end

    def probe_command(_runtime, command)
      stdout, stderr, status = Timeout.timeout(5) { Open3.capture3(*command) }
      message = [stderr, stdout].map(&:strip).reject(&:empty?).first || "command exited #{status.exitstatus}"
      Probe.new(success: status.success?, message: message)
    rescue Errno::ENOENT
      Probe.new(success: false, message: "#{command.first} is not installed")
    rescue Timeout::Error
      Probe.new(success: false, message: "probe timed out")
    end
  end
end
