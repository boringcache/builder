# frozen_string_literal: true

module BoringBuilder
  class BoringCache
    CLI_IMAGE = "ghcr.io/boringcache/base:bookworm-v1.43.3@" \
                "sha256:679a043d584350e58eebd9028a28f078453a7be1b77e8bccb96e7a05dd20a8c8"
    BUILD_IMAGE = "ghcr.io/boringcache/base:bookworm-build-v1.43.3@" \
                  "sha256:cd10bc06c8aa730d195fa3b2215e7cdad3c8391a9a6108c1e9ce5214332e249b"
    TOKEN_NAMES = %w[BORINGCACHE_RESTORE_TOKEN BORINGCACHE_SAVE_TOKEN].freeze
    ENVIRONMENT_NAMES = %w[BORINGCACHE_API_URL BORINGCACHE_DEFAULT_WORKSPACE BORINGCACHE_WORKSPACE].freeze

    attr_reader :project, :client, :environment

    def initialize(project, client, environment: ENV)
      @project = project
      @client = client
      @environment = environment
    end

    def enabled?
      token? && (workspace? || project_config?)
    end

    def prepare(container, workdir: project.application_path)
      return container unless enabled?

      container = container.with_file("/usr/local/bin/boringcache", cli_file, permissions: 0o755)
      container = container.with_file("#{workdir}/.boringcache.toml", project_config_file) if project_config?
      container = with_environment(container)
      with_secrets(container)
    end

    def wrap(command, entry: "bundler")
      return command unless enabled?

      wrapped(command, "--entry", entry)
    end

    def wrap_mount(command, tag:, path:)
      wrap_mounts(command, tag => path)
    end

    def wrap_mounts(command, mounts)
      return command unless enabled?

      cache_arguments = mounts.flat_map { |tag, path| ["--manual-entry", "#{tag}:#{path}"] }
      wrapped(command, *cache_arguments)
    end

    def mode
      enabled? ? :boringcache : :local
    end

    def artifact_publishable?
      save_token? && (workspace? || project_config?)
    end

    def prepare_artifact_publisher(container, workdir: "/workspace")
      container = container.with_workdir(workdir)
      container = container.with_file("#{workdir}/.boringcache.toml", project_config_file) if project_config?
      container = with_environment(container)
      with_secrets(container)
    end

    def cli_image
      environment.fetch("BORINGBUILDER_BORINGCACHE_IMAGE", CLI_IMAGE)
    end

    def save_token?
      !value("BORINGCACHE_SAVE_TOKEN").nil?
    end

    def workspace_configured?
      workspace? || project_config?
    end

    private

    def wrapped(command, *cache_arguments)
      arguments = ["boringcache", "run", *cache_arguments, "--no-git"]
      arguments << "--read-only" unless save_token?
      [*arguments, "--", *command]
    end

    def token?
      TOKEN_NAMES.any? { |name| value(name) }
    end

    def workspace?
      %w[BORINGCACHE_DEFAULT_WORKSPACE BORINGCACHE_WORKSPACE].any? { |name| value(name) }
    end

    def project_config?
      project.root.join(".boringcache.toml").file?
    end

    def project_config_file
      project.source_file(client, ".boringcache.toml")
    end

    def cli_file
      options = project.configuration.platform ? { platform: project.configuration.platform } : {}
      client.container(options).from(cli_image).file("/usr/local/bin/boringcache")
    end

    def with_environment(container)
      ENVIRONMENT_NAMES.reduce(container) do |result, name|
        value(name) ? result.with_env_variable(name, value(name)) : result
      end
    end

    def with_secrets(container)
      TOKEN_NAMES.reduce(container) do |result, name|
        value(name) ? result.with_secret_variable(name, secret(name)) : result
      end
    end

    def secret(name)
      client.set_secret(name, value(name))
    end

    def value(name)
      environment[name].to_s.empty? ? nil : environment[name]
    end
  end
end
