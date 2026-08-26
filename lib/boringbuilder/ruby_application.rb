# frozen_string_literal: true

module BoringBuilder
  class RubyApplication
    attr_reader :root, :configuration

    def initialize(root, configuration)
      @root = root
      @configuration = configuration
    end

    def ruby?
      root.join("Gemfile").file?
    end

    def rails?
      ruby? && root.join("config/application.rb").file?
    end

    def hanami?
      ruby? && root.join("config/app.rb").file?
    end

    def framework
      return unless ruby?
      return :rails if rails?
      return :hanami if hanami?
      return :rack if rack?

      :ruby
    end

    def path
      rails? ? "/rails" : "/app"
    end

    def user
      rails? ? "rails" : "app"
    end

    def runtime_environment
      environment = {
        "PORT" => port.to_s,
        "RACK_ENV" => "production"
      }
      environment.merge!(rails_environment) if rails?
      environment.merge!("HANAMI_ENV" => "production", "HANAMI_PORT" => port.to_s) if hanami?
      environment
    end

    def runtime_command
      configuration.command ? explicit_command : detected_command
    end

    def runtime_entrypoint
      return [] if configuration.command || procfile_command
      return %w[/rails/bin/docker-entrypoint] if rails? && docker_entrypoint?
      return %w[/rails/bin/rails] if rails?

      []
    end

    def runtime_port
      port
    end

    def web?
      rails? || hanami? || rack? || !procfile_command.nil?
    end

    private

    def detected_command
      return procfile_command if procfile_command
      return rails_command if rails?
      return hanami_command if hanami?
      return rack_command if rack?
      return %w[bundle exec rake] if root.join("Rakefile").file?

      []
    end

    def explicit_command
      command = configuration.command
      return command.map(&:to_s) if command.is_a?(Array)

      ["/bin/sh", "-lc", command.to_s]
    end

    def rails_command
      return %w[./bin/thrust ./bin/rails server] if thrust?

      docker_entrypoint? ? %w[./bin/rails server] : %w[server]
    end

    def hanami_command
      return %w[bundle exec puma -C config/puma.rb] if root.join("config/puma.rb").file?
      return rack_command if rack?

      []
    end

    def rack_command
      ["bundle", "exec", "rackup", "config.ru", "--host", "0.0.0.0", "--port", port.to_s]
    end

    def procfile_command
      return @procfile_command if defined?(@procfile_command)

      path = root.join("Procfile")
      @procfile_command = if path.file?
                            command = path.each_line.filter_map { |line| line[/\Aweb:\s*(.+)\s*\z/, 1] }.first
                            ["/bin/sh", "-lc", command] if command
                          end
    end

    def rack?
      root.join("config.ru").file?
    end

    def docker_entrypoint?
      root.join("bin/docker-entrypoint").file?
    end

    def thrust?
      root.join("bin/thrust").file?
    end

    def port
      configuration.port || (rails? && thrust? ? 80 : 3000)
    end

    def rails_environment
      {
        "NODE_ENV" => "production",
        "RAILS_ENV" => "production",
        "RAILS_LOG_TO_STDOUT" => "true",
        "RAILS_SERVE_STATIC_FILES" => "true"
      }
    end
  end
end
