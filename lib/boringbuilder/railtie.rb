# frozen_string_literal: true

module BoringBuilder
  class Railtie < Rails::Railtie
    rake_tasks do
      load File.expand_path("tasks/boringbuilder.rake", __dir__)
    end
  end
end
