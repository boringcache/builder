# frozen_string_literal: true

module BoringBuilder
  class ProjectPlan
    attr_reader :project

    def initialize(project)
      @project = project
    end

    def to_h
      identity.merge(build, artifact, delivery)
    end

    private

    def identity
      {
        app: project.app_name,
        root: project.root.to_s,
        strategy: strategy,
        framework: project.framework,
        ruby_version: ruby_version
      }
    end

    def build
      {
        platform: configuration.platform,
        runtime: configuration.runtime,
        format: configuration.format,
        command: command,
        port: port,
        cache: BoringCache.new(project, nil).mode
      }
    end

    def artifact
      {
        output: project.output_path.to_s,
        artifact: project.artifact.to_a,
        artifact_paths: project.artifact_paths,
        exporters: Exporters::Resolver.new(project, nil).plan
      }
    end

    def delivery
      {
        publish: configuration.publish,
        load: configuration.load
      }
    end

    def configuration
      project.configuration
    end

    def strategy
      return "custom" if project.custom_pipeline?

      project.framework.to_s
    end

    def ruby_version
      project.ruby? ? project.ruby_version : nil
    end

    def command
      project.custom_pipeline? ? nil : project.runtime_command
    end

    def port
      return if project.custom_pipeline? || !project.web?

      project.runtime_port
    end
  end
end
