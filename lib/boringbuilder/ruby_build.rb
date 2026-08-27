# frozen_string_literal: true

module BoringBuilder
  class RubyBuild
    DEFAULT_BUILD_IMAGE = Mise::DEFAULT_IMAGE
    DEFAULT_RUNTIME_IMAGE = BoringCache::CLI_IMAGE
    BUILD_PACKAGES = %w[
      build-essential
      curl
      git
      libpq-dev
      libyaml-dev
      pkg-config
    ].freeze
    RUNTIME_PACKAGES = %w[
      curl
      libpq5
    ].freeze
    BUNDLE_ENVIRONMENT = {
      "BUNDLE_CLEAN" => "true",
      "BUNDLE_DEPLOYMENT" => "1",
      "BUNDLE_IGNORE_CONFIG" => "true",
      "BUNDLE_PATH" => "/usr/local/bundle",
      "BUNDLE_WITHOUT" => "development:test"
    }.freeze
    BUNDLE_INSTALL_COMMAND = [
      "sh", "-c",
      'export BUNDLE_JOBS="${BUNDLE_JOBS:-$(nproc)}"; bundle install && bundle clean --force'
    ].freeze
    APT_INSTALL_COMMAND = [
      "sh", "-c",
      "apt-get update && apt-get install -y --no-install-recommends \"$@\" && rm -rf /var/lib/apt/lists/*",
      "apt-get"
    ].freeze
    BUNDLE_FILES = %w[Gemfile Gemfile.lock .ruby-version].freeze
    BUNDLE_CONFIG = <<~YAML
      ---
      BUNDLE_PATH: "vendor/bundle"
      BUNDLE_WITHOUT: "development:test"
      BUNDLE_CLEAN: "true"
    YAML

    attr_reader :project, :client, :progress

    def initialize(project, client, progress: DaggerRuby::Progress.silent)
      @project = project
      @client = client
      @progress = progress
    end

    def container
      builder = with_environment(build_image_container, build_environment)
      builder = install_packages(builder, (BUILD_PACKAGES + configuration.build_packages).uniq, stage: :build)
      builder = create_application_user(builder, stage: :build)
      builder = install_toolchain(builder)
      builder = install_gems(builder)
      builder = pipeline.step("[build] COPY application source", builder) do |container|
        container.with_directory(application_path, project.source(client), owner: user_owner)
      end
      builder = pipeline.exec(
        builder,
        %w[bundle check],
        name: "[build] RUN bundle check",
        workdir: application_path
      )
      builder = pipeline.exec(
        builder,
        %w[rm -rf .bundle/cache],
        name: "[build] RUN rm -rf .bundle/cache",
        workdir: application_path
      )
      builder = precompile(builder)
      builder = pipeline.step("[build] Write Bundler configuration", builder) { write_bundle_config(_1) }

      runtime_container(builder)
    end

    private

    def configuration
      project.configuration
    end

    def application_path
      project.application_path
    end

    def user_owner
      "#{project.application_user}:#{project.application_user}"
    end

    def image_options
      configuration.platform ? { platform: configuration.platform } : {}
    end

    def build_image_container
      address = configuration.base_image || DEFAULT_BUILD_IMAGE
      progress.step("[build] FROM #{address}") { client.container(image_options).from(address).sync }
    end

    def runtime_image_container
      address = configuration.base_image || DEFAULT_RUNTIME_IMAGE
      progress.step("[runtime] FROM #{address}") { client.container(image_options).from(address).sync }
    end

    def build_environment
      BUNDLE_ENVIRONMENT
        .merge(project.runtime_environment)
        .merge("SECRET_KEY_BASE_DUMMY" => "1")
        .merge(configuration.build_environment)
    end

    def runtime_environment
      BUNDLE_ENVIRONMENT.merge(project.runtime_environment)
    end

    def with_environment(container, environment)
      environment.reduce(container) do |result, (name, value)|
        result.with_env_variable(name, value)
      end
    end

    def install_toolchain(container)
      toolchain = Mise.new(project, pipeline)
      container = toolchain.with_project_metadata(container, at: application_path)
      toolchain.install(container, tools: { ruby: project.ruby_version }, workdir: application_path, only: :ruby)
    end

    def install_gems(container)
      container = BUNDLE_FILES.reduce(container.with_workdir(application_path)) do |result, name|
        path = project.root.join(name)
        path.file? ? result.with_file("#{application_path}/#{name}", project.source_file(client, name)) : result
      end
      container = pipeline.run(
        container,
        BUNDLE_INSTALL_COMMAND,
        cache: "bundle",
        at: "/usr/local/bundle/cache",
        entry: "bundler",
        name: "[build] RUN bundle install"
      )
      container.with_exec(["find", "/usr/local/bundle", "-path", "*/cache/*.gem", "-type", "f", "-delete"])
               .with_exec([
                            "sh", "-c",
                            "find /usr/local/bundle -path '*/bundler/gems/*/.git' -type d -prune -exec rm -rf '{}' +"
                          ])
    end

    def write_bundle_config(container)
      container.with_workdir(application_path)
               .with_exec(%w[rm -rf .bundle/cache])
               .with_exec(%w[mkdir -p .bundle])
               .with_new_file("#{application_path}/.bundle/config", BUNDLE_CONFIG)
    end

    def pipeline
      @pipeline ||= Pipeline.new(project, client, progress: progress)
    end

    def runtime_container(builder)
      container = with_environment(runtime_image_container, Mise::ENVIRONMENT.merge(runtime_environment))
      container = install_packages(container, (RUNTIME_PACKAGES + configuration.runtime_packages).uniq,
                                   stage: :runtime)
      container = create_application_user(container, stage: :runtime)
      container = pipeline.step("[runtime] COPY toolchain, gems, and application", container) do |runtime|
        runtime.with_directory("/mise/installs", builder.directory("/mise/installs"))
               .with_directory("/usr/local/bundle", builder.directory("/usr/local/bundle"), owner: user_owner)
               .with_directory(application_path, builder.directory(application_path), owner: user_owner)
      end
      container = pipeline.exec(
        container,
        %w[mise reshim],
        name: "[runtime] RUN mise reshim",
        workdir: application_path
      )
      container = runtime_metadata(container).with_user(project.application_user)
      progress.step("[runtime] Configure image") { container.sync }
    end

    def install_packages(container, packages, stage:)
      pipeline.exec(
        container,
        [*APT_INSTALL_COMMAND, *packages],
        name: "[#{stage}] RUN apt-get update && apt-get install packages"
      )
    end

    def create_application_user(container, stage:)
      name = project.application_user
      pipeline.step("[#{stage}] Create application user", container) do |step|
        step.with_exec(["groupadd", "--system", "--gid", "1000", name])
            .with_exec(["useradd", name, "--uid", "1000", "--gid", "1000", "--create-home", "--shell", "/bin/bash"])
      end
    end

    def precompile(container)
      if project.rails? && project.assets?
        container = pipeline.exec(
          container,
          %w[bin/rails assets:precompile],
          name: "[build] RUN bin/rails assets:precompile"
        )
      end
      if project.bootsnap?
        paths = project.rails? ? %w[app/ lib/] : %w[app/ lib/ config/]
        container = pipeline.exec(
          container,
          ["bundle", "exec", "bootsnap", "precompile", "--gemfile", *paths],
          name: "[build] RUN bundle exec bootsnap precompile"
        )
      end
      return container unless project.rails?

      pipeline.step("[build] Normalize Rails output", container) { normalize_rails_output(_1) }
    end

    def normalize_rails_output(container)
      ruby = <<~RUBY.strip
        path = "public/assets/.manifest.json"
        if File.file?(path)
          manifest = JSON.parse(File.binread(path))
          File.binwrite(path, JSON.generate(manifest.sort.to_h))
        end
      RUBY
      container
        .with_exec(["ruby", "-rjson", "-e", ruby])
        .with_exec(%w[rm -f tmp/local_secret.txt tmp/cache/bootsnap/load-path-cache])
    end

    def runtime_metadata(container)
      entrypoint = project.runtime_entrypoint
      command = project.runtime_command
      container = container.with_entrypoint(entrypoint) unless entrypoint.empty?
      container = container.with_default_args(command) unless command.empty?
      return container unless project.web?

      port = project.runtime_port
      container = container.with_exposed_port(port, description: "Ruby HTTP server")
      return container unless project.rails?

      container.with_docker_healthcheck(
        ["curl", "--fail", "--silent", "http://localhost:#{port}/up"],
        interval: "30s",
        timeout: "5s",
        retries: 3
      )
    end
  end
end
