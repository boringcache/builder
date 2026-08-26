# frozen_string_literal: true

require "test_helper"

class ToolchainTest < Minitest::Test
  ROOT = Pathname.new(File.expand_path("../..", __dir__))

  def test_development_ruby_is_aligned_across_executable_configuration
    ruby_version = ROOT.join(".ruby-version").read.strip
    ci = ROOT.join(".github/workflows/ci.yml").read
    publish = ROOT.join(".github/workflows/publish.yml").read

    assert_equal ruby_version, mise_version("ruby")
    assert_equal ruby_version, ROOT.join("test/fixtures/rails_app/.ruby-version").read.strip
    assert_includes ci, %(ruby-version: "#{ruby_version}")
    assert_includes publish, %(ruby-version: "#{ruby_version}")
  end

  def test_supported_ruby_floor_is_exercised_by_ci
    specification = Gem::Specification.load(ROOT.join("boringbuilder.gemspec").to_s)
    minimum = specification.required_ruby_version.requirements
                           .filter_map { |operator, version| version if %w[>= > =].include?(operator) }
                           .min
    ci_version = minimum.segments.first(2).join(".")

    assert_includes ROOT.join(".github/workflows/ci.yml").read, %(ruby: ["#{ci_version}")
  end

  def test_dagger_version_is_aligned_with_dagger_ruby
    dagger_version = DaggerRuby::DAGGER_VERSION
    ci = ROOT.join(".github/workflows/ci.yml").read

    assert_equal dagger_version, mise_version("dagger")
    assert_equal 2, ci.scan(%(version: "#{dagger_version}")).length
  end

  private

  def mise_version(tool)
    ROOT.join("mise.toml").read[/^#{Regexp.escape(tool)} = "([^"]+)"$/, 1]
  end
end
