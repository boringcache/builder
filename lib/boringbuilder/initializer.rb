# frozen_string_literal: true

require "fileutils"
require "json"

module BoringBuilder
  class Initializer
    TEMPLATES = %i[auto ruby node rust go generic].freeze

    attr_reader :root, :requested_template

    def initialize(root, template: :auto)
      @root = Pathname.new(root).expand_path
      @requested_template = template.to_s.downcase.to_sym
    end

    def call(force: false)
      validate!
      raise ConfigurationError, "#{path} already exists; pass --force to replace it" if path.exist? && !force

      FileUtils.mkdir_p(path.dirname)
      path.write(template)
      path
    end

    def template_name
      return requested_template unless requested_template == :auto
      return :ruby if root.join("Gemfile").file?
      return :node if root.join("package.json").file?
      return :rust if root.join("Cargo.toml").file?
      return :go if root.join("go.mod").file?

      :generic
    end

    private

    def path
      root.join(ConfigFile::DEFAULT_PATH)
    end

    def validate!
      raise ConfigurationError, "Project directory does not exist: #{root}" unless root.directory?
      return if TEMPLATES.include?(requested_template)

      raise ConfigurationError, "Unknown template '#{requested_template}'. Use auto, ruby, node, rust, go, or generic."
    end

    def template
      send("#{template_name}_template")
    end

    def ruby_template
      <<~RUBY
        # frozen_string_literal: true

        # Rails, Hanami, Rack, and other Bundler applications use BoringBuilder's
        # built-in Mise-powered pipeline. Add only the application-specific choices.
        BoringBuilder.configure do |config|
          # config.build_packages += %w[libexample-dev]
          # config.runtime_packages += %w[libexample1]
        end
      RUBY
    end

    def node_template
      install = root.join("package-lock.json").file? ? "%w[npm ci]" : "%w[npm install]"
      build = node_build?
      artifact = if build
                   'config.artifact.directory("/app/dist", at: "/app")'
                 else
                   'config.artifact.directory("/app")'
                 end
      build_step = if build
                     "\n    pipeline.exec(app, %w[npm run build], name: \"Build application\")"
                   else
                     "\n    app"
                   end

      <<~RUBY
        # frozen_string_literal: true

        BoringBuilder.configure do |config|
          #{artifact}

          config.pipeline do |pipeline|
            app = pipeline.mise(tools: { node: "24" }, workdir: "/app")

            app = pipeline.run(
              app,
              #{install},
              cache: "npm-downloads",
              at: "/root/.npm/_cacache",
              workdir: "/app",
              name: "Install dependencies"
            )
        #{build_step}
          end
        end
      RUBY
    end

    def rust_template
      <<~RUBY
        # frozen_string_literal: true

        BoringBuilder.configure do |config|
          config.artifact.directory("/app/target/release", at: "/app")

          config.pipeline do |pipeline|
            app = pipeline.mise(tools: { rust: "stable" }, workdir: "/app")

            pipeline.run(
              app,
              %w[cargo build --release],
              cache: {
                "cargo-registry" => "/root/.cargo/registry",
                "cargo-git" => "/root/.cargo/git"
              },
              workdir: "/app",
              name: "Build application"
            )
          end
        end
      RUBY
    end

    def go_template
      <<~RUBY
        # frozen_string_literal: true

        BoringBuilder.configure do |config|
          config.artifact.directory("/app/dist", at: "/app")

          config.pipeline do |pipeline|
            app = pipeline.mise(tools: { go: "1" }, workdir: "/app")

            pipeline.run(
              app,
              %w[go build -o /app/dist/app .],
              cache: {
                "go-modules" => "/root/go/pkg/mod",
                "go-build" => "/root/.cache/go-build"
              },
              workdir: "/app",
              name: "Build application"
            )
          end
        end
      RUBY
    end

    def generic_template
      <<~RUBY
        # frozen_string_literal: true

        BoringBuilder.configure do |config|
          config.artifact.directory("/app")

          config.pipeline do |pipeline|
            # Add only the tools the application needs, then its build steps.
            pipeline.mise(
              tools: {
                # "python" => "3"
              },
              workdir: "/app"
            )
          end
        end
      RUBY
    end

    def node_build?
      scripts = JSON.parse(root.join("package.json").read)["scripts"]
      scripts.is_a?(Hash) && scripts.key?("build")
    rescue JSON::ParserError, TypeError
      false
    end
  end
end
