# frozen_string_literal: true

module BoringBuilder
  class Project
    DEFAULT_EXCLUDES = %w[
      .env
      .env.*
      .bundle/cache
      .git
      coverage
      dist
      log
      node_modules
      tmp
      vendor/bundle
    ].freeze

    attr_reader :configuration

    def initialize(configuration)
      @configuration = configuration
    end

    def validate!
      configuration.validate!
      validate_lockfile!
      validate_application!
      validate_native_javascript!
      Exporters::Resolver.new(self, nil).validate!
      self
    end

    def container(client, progress: DaggerRuby::Progress.silent)
      return Pipeline.new(self, client, progress: progress).build(&configuration.pipeline) if custom_pipeline?

      builder = rails? ? RailsBuild : RubyBuild
      builder.new(self, client, progress: progress).container
    end

    def plan
      ProjectPlan.new(self).to_h
    end

    def root
      configuration.root
    end

    def app_name
      @app_name ||= root.basename.to_s.downcase.gsub(/[^a-z0-9]+/, "-").gsub(/\A-+|-+\z/, "")
    end

    def rails?
      application.rails?
    end

    def ruby?
      application.ruby?
    end

    def hanami?
      application.hanami?
    end

    def framework
      application.framework
    end

    def locked?
      root.join("Gemfile.lock").file?
    end

    def ruby_version
      @ruby_version ||= mise_ruby_version || tool_versions_ruby_version || ruby_version_file ||
                        gemfile_ruby_version || locked_ruby_version || RUBY_VERSION
    end

    def assets?
      return configuration.assets unless configuration.assets.nil?

      root.join("app/assets").directory? || root.join("config/manifest.js").file?
    end

    def bootsnap?
      return configuration.bootsnap unless configuration.bootsnap.nil?

      root.join("Gemfile.lock").read.match?(/^    bootsnap \(/)
    end

    def artifact_paths
      artifact.entries.filter_map { |entry| entry.source if entry.origin == :container }.map(&:to_s)
    end

    def application_path
      application.path
    end

    def application_user
      application.user
    end

    def runtime_environment
      application.runtime_environment
    end

    def runtime_command
      application.runtime_command
    end

    def runtime_entrypoint
      application.runtime_entrypoint
    end

    def runtime_port
      application.runtime_port
    end

    def web?
      application.web?
    end

    def artifact
      @artifact ||= begin
        configured = configuration.artifact
        add_configured_entries(configured)
        add_default_entries(configured) if configured.empty?
        configured
      end
    end

    def output_path
      return configuration.output if configuration.output

      suffix = {
        directory: "rootfs",
        tar: "tar",
        tar_zst: "tar.zst",
        oci: "oci.tar",
        docker: "docker.tar"
      }.fetch(configuration.format)
      platform = configuration.platform&.tr("/", "-") || "native"
      root.join("dist", "#{app_name}-#{platform}.#{suffix}")
    end

    def source(client)
      client.host.directory(root.to_s, exclude: DEFAULT_EXCLUDES, gitignore: true)
    end

    def source_file(client, path)
      client.host.file(root.join(path).to_s)
    end

    def custom_pipeline?
      !configuration.pipeline.nil?
    end

    private

    def validate_lockfile!
      return if custom_pipeline? || !ruby? || locked?

      raise ConfigurationError, "Gemfile.lock is required for a reproducible Ruby build"
    end

    def validate_application!
      return if custom_pipeline? || ruby?

      raise ConfigurationError, "No conventional Ruby application or custom pipeline found in #{root}"
    end

    def add_configured_entries(artifact)
      configuration.paths.each do |mapping|
        source, destination = parse_mapping(mapping)
        artifact.directory(source, at: destination)
      end
      configuration.files.each do |mapping|
        source, destination = parse_mapping(mapping)
        artifact.file(source, at: destination)
      end
      configuration.host_paths.each do |mapping|
        source, destination = parse_mapping(mapping, destination_required: true)
        artifact.host_path(source, at: destination)
      end
    end

    def add_default_entries(artifact)
      paths = custom_pipeline? ? ["/"] : [application_path, "/usr/local", "/mise"]
      paths.each { |path| artifact.directory(path) }
    end

    def parse_mapping(mapping, destination_required: false)
      source, destination = mapping.to_s.split("=", 2)
      if source.to_s.empty? || (destination_required && destination.to_s.empty?)
        raise ConfigurationError, "Artifact mapping must be SOURCE=DESTINATION: #{mapping.inspect}"
      end

      [source, destination || source]
    end

    def validate_native_javascript!
      return if custom_pipeline? || !root.join("package.json").file?

      raise ConfigurationError, <<~MESSAGE.strip
        package.json was found. Define config/boringbuilder.rb with a custom Dagger pipeline that installs the
        application's JavaScript runtime.
      MESSAGE
    end

    def application
      @application ||= RubyApplication.new(root, configuration)
    end

    def ruby_version_file
      path = root.join(".ruby-version")
      return unless path.file?

      normalize_ruby_version(path.read.strip)
    end

    def mise_ruby_version
      path = %w[mise.toml .mise.toml].map { |name| root.join(name) }.find(&:file?)
      return unless path

      tools_section = false
      path.each_line do |line|
        value = line.strip.sub(/\s+#.*\z/, "")
        if value.start_with?("[")
          tools_section = value == "[tools]"
          next
        end
        next unless tools_section

        version = value[/\Aruby\s*=\s*["']([^"']+)["']\z/, 1]
        return normalize_ruby_version(version) if version
      end

      nil
    end

    def tool_versions_ruby_version
      path = root.join(".tool-versions")
      return unless path.file?

      version = path.each_line.filter_map { |line| line[/\Aruby\s+(\S+)/, 1] }.first
      normalize_ruby_version(version) if version
    end

    def gemfile_ruby_version
      path = root.join("Gemfile")
      return unless path.file?

      version = path.read[/^\s*ruby\s+["']([^"']+)["']/, 1]
      normalize_ruby_version(version) if version
    end

    def normalize_ruby_version(version)
      version.to_s.delete_prefix("ruby-")
    end

    def locked_ruby_version
      return unless locked?

      root.join("Gemfile.lock").read[/^RUBY VERSION\n\s+ruby ([^\s]+)/, 1]&.sub(/p\d+\z/, "")
    end
  end
end
