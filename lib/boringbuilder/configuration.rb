# frozen_string_literal: true

require "pathname"

module BoringBuilder
  class Configuration
    FORMATS = %i[directory tar tar_zst oci docker].freeze
    RUNTIMES = %i[auto apple docker].freeze
    FORMAT_ALIASES = {
      "tar.zst" => :tar_zst,
      "tar-zst" => :tar_zst,
      "tar_zst" => :tar_zst
    }.freeze

    attr_accessor :root, :runtime, :platform, :format, :output,
                  :base_image, :paths, :files, :host_paths, :publish, :load,
                  :progress, :assets, :bootsnap, :artifact,
                  :command, :port, :exporter, :artifact_name,
                  :build_packages, :runtime_packages, :build_environment
    attr_writer :pipeline

    def initialize(root: Dir.pwd, runtime: :auto, platform: nil, format: :tar_zst,
                   output: nil, base_image: nil, paths: nil,
                   files: nil, host_paths: nil, publish: nil, load: nil,
                   progress: nil, assets: nil, bootsnap: nil,
                   artifact: nil, command: nil, port: nil,
                   exporter: :auto, artifact_name: nil, pipeline: nil,
                   build_packages: nil, runtime_packages: nil, build_environment: nil)
      @root = Pathname.new(root).expand_path
      @runtime = normalize_runtime(runtime)
      @platform = platform
      @format = normalize_format(format)
      @output = output && Pathname.new(output).expand_path(@root)
      @base_image = base_image
      @paths = Array(paths).compact
      @files = Array(files).compact
      @host_paths = Array(host_paths).compact
      @publish = publish
      @load = load
      @progress = progress
      @assets = assets
      @bootsnap = bootsnap
      @artifact = artifact || Artifact.new(root: @root)
      @command = command
      @port = port
      @exporter = normalize_exporter(exporter)
      @artifact_name = artifact_name
      @pipeline = pipeline
      @build_packages = normalize_packages(build_packages)
      @runtime_packages = normalize_packages(runtime_packages)
      @build_environment = normalize_environment(build_environment)
    end

    def validate!
      normalize!
      validate_project_directory!
      validate_option_combinations!
      validate_port!
      validate_artifact!
      validate_output_path!
      self
    end

    def apply(options)
      options.each do |name, value|
        writer = "#{name}="
        raise ConfigurationError, "Unknown configuration option: #{name}" unless respond_to?(writer)

        public_send(writer, value)
      end
      self
    end

    def pipeline(&block)
      @pipeline = block if block
      @pipeline
    end

    private

    def validate_project_directory!
      raise ConfigurationError, "Project directory does not exist: #{root}" unless root.directory?
    end

    def validate_option_combinations!
      raise ConfigurationError, "--push and --load cannot be used together" if publish && load
    end

    def validate_port!
      raise ConfigurationError, "port must be a positive integer" if port && port <= 0
    end

    def validate_artifact!
      raise ConfigurationError, "artifact must be a BoringBuilder::Artifact" unless artifact.is_a?(Artifact)
      raise ConfigurationError, "pipeline must be callable" if pipeline && !pipeline.respond_to?(:call)
    end

    def normalize!
      @root = Pathname.new(root).expand_path
      @runtime = normalize_runtime(runtime)
      @format = normalize_format(format)
      @output = output && Pathname.new(output).expand_path(root)
      normalize_collection_options!
      normalize_build_options!
    end

    def normalize_collection_options!
      @paths = Array(paths).compact
      @files = Array(files).compact
      @host_paths = Array(host_paths).compact
    end

    def normalize_build_options!
      @port = normalize_port(port) if port
      @exporter = normalize_exporter(exporter)
      @artifact_name = artifact_name&.to_s
      @build_packages = normalize_packages(build_packages)
      @runtime_packages = normalize_packages(runtime_packages)
      @build_environment = normalize_environment(build_environment)
    end

    def normalize_runtime(value)
      runtime = value.to_s.downcase.to_sym
      return runtime if RUNTIMES.include?(runtime)

      raise ConfigurationError, "Unknown runtime '#{value}'. Use auto, apple, or docker."
    end

    def normalize_format(value)
      format = FORMAT_ALIASES.fetch(value.to_s, value.to_s.downcase.to_sym)
      return format if FORMATS.include?(format)

      raise ConfigurationError, "Unknown format '#{value}'. Use directory, tar, tar.zst, oci, or docker."
    end

    def normalize_port(value)
      Integer(value)
    rescue ArgumentError, TypeError
      raise ConfigurationError, "port must be a positive integer"
    end

    def normalize_exporter(value)
      exporter = (value || :auto).to_sym
      return exporter if %i[auto local boringcache].include?(exporter)

      raise ConfigurationError, "exporter must be auto, local, or boringcache"
    end

    def normalize_packages(packages)
      Array(packages).compact.map(&:to_s)
    end

    def normalize_environment(environment)
      (environment || {}).to_h { |name, value| [name.to_s, value.to_s] }
    end

    def validate_output_path!
      return unless output

      unsafe = output == Pathname.new("/") || output == root || output == Pathname.new(Dir.home).expand_path
      unsafe ||= format == :directory && root.to_s.start_with?("#{output}/")
      raise ConfigurationError, "Refusing unsafe output path: #{output}" if unsafe
    end
  end
end
