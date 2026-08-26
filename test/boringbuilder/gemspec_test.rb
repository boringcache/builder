# frozen_string_literal: true

require "test_helper"

class GemspecTest < Minitest::Test
  def test_gem_metadata_and_runtime_contract
    specification = Gem::Specification.load(File.expand_path("../../boringbuilder.gemspec", __dir__))

    assert_equal "boringbuilder", specification.name
    assert_equal Gem::Version.new(BoringBuilder::VERSION), specification.version
    assert_operator specification.required_ruby_version, :satisfied_by?, Gem::Version.new("3.2.0")
    assert_equal "true", specification.metadata.fetch("rubygems_mfa_required")
    assert(specification.dependencies.any? { |dependency| dependency.name == "dagger_ruby" })
  end
end
