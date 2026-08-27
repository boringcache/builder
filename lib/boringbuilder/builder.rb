# frozen_string_literal: true

require "dagger_ruby"

module BoringBuilder
  class Builder
    attr_reader :configuration, :progress

    def initialize(configuration, progress: DaggerRuby::Progress.silent)
      @configuration = configuration
      @progress = progress
    end

    def build
      project = Project.new(configuration).validate!
      runtime = Runtime.new(configuration.runtime)
      resolved_runtime = runtime.resolve
      dagger_configuration = DaggerRuby::Config.new(
        runtime: runtime.dagger_runtime(resolved_runtime),
        progress: configuration.progress,
        silent: configuration.progress.nil?
      )

      DaggerRuby.connection(dagger_configuration) do |client|
        container = project.container(client, progress: progress)
        Exporter.new(project, client, container, runtime: resolved_runtime, progress: progress).call
      end
    rescue DaggerRuby::DaggerError => e
      message = e.message.sub(/\s*\[traceparent:[^\]]+\]\s*\z/, "")
      raise BuildError, "Dagger build failed: #{message}"
    end
  end
end
