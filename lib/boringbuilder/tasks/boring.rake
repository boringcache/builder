# frozen_string_literal: true

namespace :boring do
  desc "Build the Rails application with Dagger"
  task build: :environment do
    command = ["build"]
    {
      "--runtime" => "BORINGBUILDER_RUNTIME",
      "--platform" => "BORINGBUILDER_PLATFORM",
      "--format" => "BORINGBUILDER_FORMAT",
      "--output" => "BORINGBUILDER_OUTPUT",
      "--push" => "BORINGBUILDER_PUSH",
      "--load" => "BORINGBUILDER_LOAD",
      "--progress" => "BORINGBUILDER_PROGRESS",
      "--exporter" => "BORINGBUILDER_EXPORTER",
      "--artifact-name" => "BORINGBUILDER_ARTIFACT_NAME"
    }.each do |option, environment_name|
      value = ENV.fetch(environment_name, nil)
      command.push(option, value) if value && !value.empty?
    end

    {
      "--path" => "BORINGBUILDER_PATHS",
      "--file" => "BORINGBUILDER_FILES",
      "--host-path" => "BORINGBUILDER_HOST_PATHS"
    }.each do |option, environment_name|
      values = ENV.fetch(environment_name, "").split(",").reject(&:empty?)
      values.each { |value| command.push(option, value) }
    end

    command << Rails.root.to_s
    Kernel.exec(Gem.ruby, Gem.bin_path("boringbuilder", "boringbuilder"), *command)
  end

  desc "Check the configured container runtime"
  task :doctor do
    requested = ENV.fetch("BORINGBUILDER_RUNTIME", "auto")
    runtime = BoringBuilder::Runtime.new(requested).resolve
    puts "Dagger runtime ready: #{runtime}"
  end
end
