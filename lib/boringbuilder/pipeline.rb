# frozen_string_literal: true

module BoringBuilder
  class Pipeline
    attr_reader :project, :client, :progress

    def initialize(project, client, environment: ENV, progress: BuildProgress.silent)
      @project = project
      @client = client
      @environment = environment
      @progress = progress
    end

    def build
      container = yield self
      raise ConfigurationError, "The custom pipeline must return a Dagger container" unless container

      sync_step(container, "Build application")
    end

    def container(image = nil)
      result = client.container(container_options)
      image ? result.from(image) : result
    end

    def source
      project.source(client)
    end

    def mise(tools: {}, image: Mise::DEFAULT_IMAGE, workdir: project.application_path)
      Mise.new(project, self).container(tools: tools, image: image, workdir: workdir)
    end

    def run(container, command, cache:, at: nil, entry: nil, workdir: project.application_path, sharing: :LOCKED,
            name: "Run cached step", **options)
      arguments = Array(command).map(&:to_s)
      mounts = normalize_mounts(cache, at, entry)
      validate_step!(arguments, mounts)

      remote_cache = BoringCache.new(project, client, environment: @environment)
      container = container.with_workdir(workdir.to_s)

      result, executed = if remote_cache.enabled?
                           container = remote_cache.prepare(container, workdir: workdir)
                           command = if entry
                                       remote_cache.wrap(arguments, entry: entry.to_s)
                                     else
                                       remote_cache.wrap_mounts(arguments, mounts)
                                     end
                           command_result = execute(container, command, options)
                           final_result = entry ? command_result : remove_directories(command_result, mounts.values)
                           [final_result, command_result]
                         else
                           run_with_local_cache(container, arguments, mounts, sharing, options)
                         end

      sync_step(result, name, output: -> { command_output(executed) })
    end

    def exec(container, command, name:, workdir: project.application_path, **options)
      arguments = Array(command).map(&:to_s)
      raise ConfigurationError, "step command cannot be empty" if arguments.empty?

      result = execute(container.with_workdir(workdir.to_s), arguments, options)
      sync_step(result, name, output: -> { command_output(result) })
    end

    def step(name, container)
      result = yield container
      raise ConfigurationError, "The pipeline step must return a Dagger container" unless result

      sync_step(result, name)
    end

    private

    def container_options
      project.configuration.platform ? { platform: project.configuration.platform } : {}
    end

    def normalize_mounts(cache, at, entry)
      return { cache.to_s => at.to_s } unless cache.is_a?(Hash)

      raise ConfigurationError, "at and entry apply only to a single cache" if at || entry

      cache.to_h { |name, path| [name.to_s, path.to_s] }
    end

    def mount_directly(container, mounts, sharing)
      mounts.reduce(container) do |step, (name, path)|
        step.with_mounted_cache(path, cache_volume(name), sharing: sharing)
      end
    end

    def run_with_local_cache(container, arguments, mounts, sharing, options)
      executed = execute(mount_directly(container, mounts, sharing), arguments, options)
      [remove_local_mounts(executed, mounts), executed]
    end

    def execute(container, command, options)
      options.empty? ? container.with_exec(command) : container.with_exec(command, options)
    end

    def command_output(container)
      [container.stdout, container.stderr].map(&:strip).reject(&:empty?).join("\n")
    end

    def remove_local_mounts(container, mounts)
      mounts.reduce(container) do |step, (_name, path)|
        step.chain_operation("withoutMount", { "path" => path })
            .with_exec(["rm", "-rf", path])
      end
    end

    def remove_directories(container, paths)
      container.with_exec(["rm", "-rf", *paths])
    end

    def cache_volume(name)
      client.cache_volume("boringbuilder-#{project.app_name}-#{name}")
    end

    def sync_step(container, name, output: nil)
      return container if container.equal?(@last_synced_container)

      @last_synced_container = progress.step(name) do
        progress.write { output.call } if output
        container.sync
      end
    end

    def validate_step!(arguments, mounts)
      raise ConfigurationError, "cached command cannot be empty" if arguments.empty?
      raise ConfigurationError, "at is required for a single cache" if mounts.values == [""]
      raise ConfigurationError, "at least one cache is required" if mounts.empty?

      mounts.each do |name, path|
        validate_cache_name!(name)
        validate_cache_path!(path)
      end
    end

    def validate_cache_name!(name)
      return if name.match?(/\A[a-zA-Z0-9][a-zA-Z0-9._-]*\z/)

      raise ConfigurationError, "cache name may contain only letters, numbers, '.', '_', and '-'"
    end

    def validate_cache_path!(path)
      clean = Pathname.new(path).cleanpath.to_s
      return if path.start_with?("/") && path != "/" && clean == path

      raise ConfigurationError, "cache path must be a clean absolute path other than '/'"
    end
  end
end
