# frozen_string_literal: true

module BoringBuilder
  class BuildProgress
    SPINNER_FRAMES = %w[⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏].freeze

    def self.silent
      @silent ||= new(enabled: false)
    end

    def initialize(out: $stdout, enabled: true, animated: false)
      @out = out
      @enabled = enabled
      @animated = animated
      @step = 0
    end

    def step(name)
      return yield unless @enabled

      number = next_step
      started_at = monotonic_time
      @out.puts "##{number} #{name}"
      @active_spinner = start_spinner(number, started_at)
      result = yield
      stop_active_spinner
      @out.puts "##{number} DONE #{elapsed_since(started_at)}"
      result
    rescue StandardError
      stop_active_spinner
      @out.puts "##{number} ERROR #{elapsed_since(started_at)}" if number
      raise
    ensure
      stop_active_spinner
    end

    def write(output = nil)
      return unless @enabled

      output = yield if block_given?
      stop_active_spinner
      text = output.to_s
      text.each_line { |line| @out.print "    #{line}" }
      @out.puts unless text.empty? || text.end_with?("\n")
    end

    private

    def next_step
      @step += 1
    end

    def start_spinner(number, started_at)
      return unless @animated

      Thread.new do
        frame = 0
        loop do
          @out.print "\r##{number} #{SPINNER_FRAMES.fetch(frame % SPINNER_FRAMES.length)} #{elapsed_since(started_at)}"
          @out.flush
          frame += 1
          sleep 0.1
        end
      rescue IOError
        nil
      end
    end

    def stop_spinner(thread)
      return unless thread&.alive?

      thread.kill
      thread.join
      @out.print "\r\e[2K"
    end

    def stop_active_spinner
      stop_spinner(@active_spinner)
      @active_spinner = nil
    end

    def monotonic_time
      Process.clock_gettime(Process::CLOCK_MONOTONIC)
    end

    def elapsed_since(started_at)
      format("%.1fs", monotonic_time - started_at)
    end
  end
end
