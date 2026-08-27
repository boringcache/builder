# frozen_string_literal: true

require "json"
require "optparse"
require "pathname"

module BoringBuilder
  class CLI
    def self.start(argv = ARGV, **)
      new(argv, **).start
    end

    def initialize(argv, out: $stdout, err: $stderr, builder_class: Builder, environment: ENV)
      @argv = argv.dup
      @out = out
      @err = err
      @builder_class = builder_class
      @environment = environment
    end

    def run
      return root_help if @argv.empty? || %w[help --help -h].include?(@argv.first)
      return version if %w[version --version -v].include?(@argv.first)

      command = @argv.shift
      case command
      when "build" then build
      when "doctor" then doctor
      when "init" then initialize_project
      else raise ConfigurationError, "Unknown command '#{command}'. Use build, doctor, init, or version."
      end
    end

    def start
      run
    rescue BoringBuilder::Error, OptionParser::ParseError => e
      @err.puts "boringbuilder: #{e.message}"
      1
    end

    private

    def version
      @out.puts BoringBuilder::VERSION
      0
    end

    def root_help
      @out.puts <<~HELP
        Usage: boringbuilder COMMAND [options]

        Commands:
          build [PROJECT]  Build and export a deployment artifact
          doctor          Check the selected container runtime
          init [PROJECT]   Generate a Dagger Ruby build recipe
          version         Print the gem version
      HELP
      0
    end

    def build
      options = { progress: "pretty" }
      config_file = :auto
      dry_run = false
      json = false
      parser = build_parser(options, ->(value) { config_file = value }, -> { dry_run = true }) { json = true }
      help_requested = catch(:boringbuilder_help) do
        parser.parse!(@argv)
        false
      end
      return 0 if help_requested
      raise OptionParser::InvalidArgument, "expected at most one PROJECT directory" if @argv.length > 1

      options[:progress] = nil if json

      configuration = BoringBuilder.configuration(root: @argv.first || Dir.pwd, config: config_file, **options)
      project = Project.new(configuration).validate!
      if dry_run
        @out.puts JSON.pretty_generate(project.plan)
        return 0
      end

      progress = build_progress(configuration, json: json)
      print_build_start(project, json: json, progress: progress)
      result = @builder_class.new(configuration, progress: progress).build
      json ? @out.puts(JSON.generate(result.to_h)) : print_result(result)
      0
    end

    def build_parser(options, config_file, dry_run, &json)
      OptionParser.new do |parser|
        parser.banner = "Usage: boringbuilder build [options] [PROJECT]"
        add_runtime_options(parser, options)
        add_build_options(parser, options)
        add_export_options(parser, options)
        add_rails_options(parser, options)
        parser.on("--config FILE", "Load a Ruby build recipe (default: config/boringbuilder.rb)") do |value|
          config_file.call(value)
        end
        parser.on("--no-config", "Ignore config/boringbuilder.rb") { config_file.call(false) }
        parser.on("--dry-run", "Print the resolved build plan") { dry_run.call }
        parser.on("--json", "Print the build result as JSON") { json.call }
        parser.on("-h", "--help", "Show this help") do
          @out.puts(parser)
          throw :boringbuilder_help, true
        end
      end
    end

    def add_runtime_options(parser, options)
      parser.on("--runtime NAME", "auto, apple, or docker") { |value| options[:runtime] = value }
      parser.on("--platform PLATFORM", "Target platform, for example linux/amd64") do |value|
        options[:platform] = value
      end
      parser.on("--progress MODE", %w[pretty auto plain tty dots logs],
                "Build progress: pretty (default), auto, plain, tty, dots, or logs") do |value|
        options[:progress] = value
      end
    end

    def add_build_options(parser, options)
      parser.on("--command COMMAND", "Override the built-in Ruby image command") { |value| options[:command] = value }
      parser.on("--port PORT", Integer, "Expose PORT for a built-in Ruby web application") do |value|
        options[:port] = value
      end
    end

    def add_export_options(parser, options)
      parser.on("--format FORMAT", "directory, tar, tar.zst, oci, or docker") { |value| options[:format] = value }
      parser.on("-o", "--output PATH", "Local artifact destination") { |value| options[:output] = value }
      parser.on("--path SOURCE[=DESTINATION]",
                "Export a container directory, optionally remapped (repeatable)") do |value|
        (options[:paths] ||= []) << value
      end
      parser.on("--file SOURCE[=DESTINATION]", "Export a container file, optionally remapped (repeatable)") do |value|
        (options[:files] ||= []) << value
      end
      parser.on("--host-path SOURCE=DESTINATION", "Add an explicit host file or directory (repeatable)") do |value|
        (options[:host_paths] ||= []) << value
      end
      parser.on("--push IMAGE", "Publish the image to a registry") { |value| options[:publish] = value }
      parser.on("--load IMAGE", "Load the image into the selected runtime") { |value| options[:load] = value }
      parser.on("--exporter NAME", "Artifact exporter: auto, local, or boringcache") do |value|
        options[:exporter] = value
      end
      parser.on("--artifact", "Use the BoringCache Artifact exporter") { options[:exporter] = :boringcache }
      parser.on("--no-artifact", "Use the local artifact exporter") { options[:exporter] = :local }
      parser.on("--artifact-name NAME", "Name the BoringCache Artifact") { |value| options[:artifact_name] = value }
    end

    def add_rails_options(parser, options)
      parser.on("--base-image IMAGE", "Base image for the built-in Rails build") do |value|
        options[:base_image] = value
      end
      parser.on("--[no-]assets", "Enable or disable Rails asset precompilation") { |value| options[:assets] = value }
      parser.on("--[no-]bootsnap", "Enable or disable Bootsnap precompilation") { |value| options[:bootsnap] = value }
    end

    def doctor
      options = { runtime: :auto }
      parser = OptionParser.new do |command|
        command.banner = "Usage: boringbuilder doctor [--runtime NAME]"
        command.on("--runtime NAME", "auto, apple, or docker") { |value| options[:runtime] = value }
        command.on("-h", "--help", "Show this help") do
          @out.puts(command)
          throw :boringbuilder_help, true
        end
      end
      help_requested = catch(:boringbuilder_help) do
        parser.parse!(@argv)
        false
      end
      return 0 if help_requested
      raise OptionParser::InvalidArgument, "doctor does not accept a PROJECT directory" unless @argv.empty?

      runtime = Runtime.new(options[:runtime]).resolve
      @out.puts "Dagger runtime ready: #{runtime}"
      0
    end

    def initialize_project
      options = { template: :auto, force: false }
      parser = OptionParser.new do |command|
        command.banner = "Usage: boringbuilder init [options] [PROJECT]"
        command.on("--template NAME", Initializer::TEMPLATES.map(&:to_s),
                   "Recipe template: auto, ruby, node, rust, go, or generic") do |value|
          options[:template] = value
        end
        command.on("--force", "Replace an existing config/boringbuilder.rb") { options[:force] = true }
        command.on("-h", "--help", "Show this help") do
          @out.puts(command)
          throw :boringbuilder_help, true
        end
      end
      help_requested = catch(:boringbuilder_help) do
        parser.parse!(@argv)
        false
      end
      return 0 if help_requested
      raise OptionParser::InvalidArgument, "expected at most one PROJECT directory" if @argv.length > 1

      root = Pathname.new(@argv.first || Dir.pwd).expand_path
      initializer = Initializer.new(root, template: options[:template])
      path = initializer.call(force: options[:force])
      @out.puts "Created #{path.relative_path_from(root)} (#{initializer.template_name})"
      0
    end

    def print_result(result)
      @out.puts "Built with Dagger on #{result.runtime}"
      @out.puts "Exported #{result.format}: #{result.path}" if result.exported?
      @out.puts "Image: #{result.reference}" if result.published?
      @out.puts "Artifact: #{result.artifact_id} (#{result.artifact_name})" if result.artifact_published?
    end

    def print_build_start(project, json:, progress:)
      return if json || !dagger_session?

      cache = BoringCache.new(project, nil, environment: @environment).mode
      pipeline = project.custom_pipeline? ? "custom pipeline" : project.framework.to_s
      details = [pipeline, "#{project.configuration.runtime} runtime", "#{cache} cache",
                 "#{project.configuration.format.to_s.tr('_', '.')} output"]

      progress.message("Building #{project.app_name} with Dagger\n  #{details.join(' · ')}")
    end

    def dagger_session?
      @environment["DAGGER_SESSION_PORT"] && @environment["DAGGER_SESSION_TOKEN"]
    end

    def build_progress(configuration, json:)
      config = DaggerRuby::Config.new(progress: configuration.progress, verify_version: false)
      config.progress_reporter(out: @out, enabled: !json && dagger_session?, environment: @environment)
    end
  end
end
