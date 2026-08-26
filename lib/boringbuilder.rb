# frozen_string_literal: true

require_relative "boringbuilder/version"
require_relative "boringbuilder/errors"
require_relative "boringbuilder/artifact"
require_relative "boringbuilder/configuration"
require_relative "boringbuilder/config_file"
require_relative "boringbuilder/initializer"
require_relative "boringbuilder/build_progress"
require_relative "boringbuilder/boring_cache"
require_relative "boringbuilder/mise"
require_relative "boringbuilder/pipeline"
require_relative "boringbuilder/runtime"
require_relative "boringbuilder/ruby_application"
require_relative "boringbuilder/project"
require_relative "boringbuilder/exporters"
require_relative "boringbuilder/project_plan"
require_relative "boringbuilder/ruby_build"
require_relative "boringbuilder/rails_build"
require_relative "boringbuilder/exporter"
require_relative "boringbuilder/result"
require_relative "boringbuilder/builder"
require_relative "boringbuilder/cli"
require_relative "boringbuilder/railtie" if defined?(Rails::Railtie)

module BoringBuilder
  class << self
    def configure(&)
      ConfigFile.configure(&)
    end

    def configuration(root: Dir.pwd, config: :auto, **options)
      configuration = Configuration.new(root: root)
      config_path = ConfigFile.resolve(configuration.root, config)
      ConfigFile.load(config_path, configuration) if config_path
      configuration.apply(options)
    end

    def build(root: Dir.pwd, config: :auto, **)
      configuration = configuration(root: root, config: config, **)
      yield configuration if block_given?
      Builder.new(configuration).build
    end
  end
end
