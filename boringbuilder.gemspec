# frozen_string_literal: true

require_relative "lib/boringbuilder/version"

Gem::Specification.new do |spec|
  spec.name = "boringbuilder"
  spec.version = BoringBuilder::VERSION
  spec.authors = ["BoringCache"]
  spec.email = ["oss@boringcache.com"]

  spec.summary = "Build deployment artifacts and images with Dagger"
  spec.description = "BoringBuilder turns application source into filesystem artifacts and container images. " \
                     "Rails is first class, while Mise-powered Dagger Ruby recipes support any workload."
  spec.homepage = "https://github.com/boringcache/builder"
  spec.license = "MIT"
  spec.required_ruby_version = ">= 3.2.0"

  spec.metadata["allowed_push_host"] = "https://rubygems.org"
  spec.metadata["homepage_uri"] = spec.homepage
  spec.metadata["source_code_uri"] = "#{spec.homepage}/tree/v#{spec.version}"
  spec.metadata["documentation_uri"] = "#{spec.homepage}#readme"
  spec.metadata["changelog_uri"] = "#{spec.homepage}/blob/main/CHANGELOG.md"
  spec.metadata["bug_tracker_uri"] = "#{spec.homepage}/issues"
  spec.metadata["security_policy_uri"] = "#{spec.homepage}/security/policy"
  spec.metadata["rubygems_mfa_required"] = "true"

  spec.files = Dir.glob(%w[
                          exe/*
                          lib/**/*
                          docs/**/*
                          AGENTS.md
                          CHANGELOG.md
                          LICENSE
                          README.md
                          SECURITY.md
                          SOUL.md
                          STYLE.md
                        ]).reject { |path| File.directory?(path) }
  spec.bindir = "exe"
  spec.executables = ["boringbuilder"]
  spec.require_paths = ["lib"]

  spec.add_dependency "dagger_ruby", "~> 0.10"
end
