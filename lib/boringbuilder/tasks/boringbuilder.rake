# frozen_string_literal: true

namespace :boringbuilder do
  desc "Build the Rails application with Dagger"
  task build: :environment do
    options = {}
    {
      runtime: "BORINGBUILDER_RUNTIME",
      platform: "BORINGBUILDER_PLATFORM",
      format: "BORINGBUILDER_FORMAT",
      output: "BORINGBUILDER_OUTPUT",
      publish: "BORINGBUILDER_PUSH",
      load: "BORINGBUILDER_LOAD"
    }.each do |option, environment_name|
      value = ENV.fetch(environment_name, nil)
      options[option] = value if value && !value.empty?
    end

    {
      paths: "BORINGBUILDER_PATHS",
      files: "BORINGBUILDER_FILES",
      host_paths: "BORINGBUILDER_HOST_PATHS"
    }.each do |option, environment_name|
      values = ENV.fetch(environment_name, "").split(",").reject(&:empty?)
      options[option] = values unless values.empty?
    end

    result = BoringBuilder.build(root: Rails.root, **options)
    puts "Built with Dagger on #{result.runtime}"
    puts "Exported #{result.format}: #{result.path}" if result.exported?
    puts "Image: #{result.reference}" if result.published?
  end

  desc "Check the configured container runtime"
  task :doctor do
    requested = ENV.fetch("BORINGBUILDER_RUNTIME", "auto")
    runtime = BoringBuilder::Runtime.new(requested).resolve
    puts "Dagger runtime ready: #{runtime}"
  end
end
