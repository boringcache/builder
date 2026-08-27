# frozen_string_literal: true

require "json"

module BoringBuilder
  class Mise
    DEFAULT_IMAGE = BoringCache::BUILD_IMAGE
    CACHE_PATH = "/mise/cache"
    INSTALL_COMMAND = %w[mise install].freeze
    METADATA_FILES = %w[mise.toml .mise.toml .tool-versions mise.lock].freeze
    ENVIRONMENT = {
      "MISE_CACHE_DIR" => CACHE_PATH,
      "MISE_DATA_DIR" => "/mise",
      "MISE_INSTALLS_DIR" => "/mise/installs",
      "MISE_SHIMS_DIR" => "/mise/shims",
      "PATH" => "/mise/shims:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    }.freeze

    attr_reader :project, :pipeline

    def initialize(project, pipeline)
      @project = project
      @pipeline = pipeline
    end

    def container(tools: {}, image: DEFAULT_IMAGE, workdir: project.application_path)
      base = with_project_metadata(pipeline.container(image), at: workdir)
      install(base, tools: tools, workdir: workdir).with_directory(workdir, pipeline.source)
    end

    def install(container, tools:, workdir:, only: nil)
      container = with_environment(container)
      container = container.with_workdir(workdir)
                           .with_new_file("/etc/mise/config.toml", system_config(tools))
      command = [*INSTALL_COMMAND, *Array(only).map(&:to_s)]
      pipeline.run(
        container,
        command,
        cache: "mise",
        at: CACHE_PATH,
        entry: "mise",
        workdir: workdir,
        name: "[build] RUN #{command.join(' ')}"
      )
    end

    def with_project_metadata(container, at: project.application_path)
      METADATA_FILES.reduce(container) do |result, name|
        path = project.root.join(name)
        path.file? ? result.with_file("#{at}/#{name}", project.source_file(pipeline.client, name)) : result
      end
    end

    def with_environment(container)
      ENVIRONMENT.reduce(container) do |result, (name, value)|
        result.with_env_variable(name, value)
      end
    end

    private

    def system_config(tools)
      declarations = normalize_tools(tools).sort.map do |name, version|
        "#{JSON.generate(name)} = #{JSON.generate(version)}"
      end

      <<~TOML
        [tools]
        #{declarations.join("\n")}

        [settings]
        paranoid = true
      TOML
    end

    def normalize_tools(tools)
      tools.to_h.each_with_object({}) do |(name, version), normalized|
        name = name.to_s
        version = version.to_s
        raise ConfigurationError, "Mise tool names and versions cannot be empty" if name.empty? || version.empty?

        normalized[name] = version
      end
    rescue NoMethodError, TypeError
      raise ConfigurationError, "Mise tools must be a name-to-version map"
    end
  end
end
