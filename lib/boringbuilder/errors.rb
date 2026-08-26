# frozen_string_literal: true

module BoringBuilder
  class Error < StandardError; end
  class ConfigurationError < Error; end
  class RuntimeError < Error; end
  class BuildError < Error; end
  class ExportError < Error; end
end
