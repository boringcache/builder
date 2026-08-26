# frozen_string_literal: true

module BoringBuilder
  class ConfigFile
    DEFAULT_PATH = "config/boringbuilder.rb"
    THREAD_KEY = :boringbuilder_config_file

    class << self
      def resolve(root, requested = :auto)
        return if requested == false || requested.nil?

        path = root.join(requested == :auto ? DEFAULT_PATH : requested).expand_path
        return path if path.file?
        return if requested == :auto

        raise ConfigurationError, "BoringBuilder config file does not exist: #{path}"
      end

      def load(path, configuration)
        previous = Thread.current[THREAD_KEY]
        state = { configuration: configuration, applied: false }
        Thread.current[THREAD_KEY] = state
        Kernel.load(path.to_s)
        raise ConfigurationError, "#{path} must call BoringBuilder.configure" unless state[:applied]

        configuration
      ensure
        Thread.current[THREAD_KEY] = previous
      end

      def configure
        state = Thread.current[THREAD_KEY]
        raise ConfigurationError, "BoringBuilder.configure is only available while loading a config file" unless state

        state[:applied] = true
        yield state.fetch(:configuration)
      end
    end
  end
end
