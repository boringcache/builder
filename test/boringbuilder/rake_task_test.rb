# frozen_string_literal: true

require "test_helper"
require "rake"

class RakeTaskTest < Minitest::Test
  def setup
    super
    @original_rake_application = Rake.application
    Rake.application = Rake::Application.new
    Rake::Task.define_task(:environment)
    load File.expand_path("../../lib/boringbuilder/tasks/boring.rake", __dir__)
  end

  def teardown
    Rake.application = @original_rake_application
    super
  end

  def test_exposes_the_rails_shaped_build_task
    assert Rake::Task.task_defined?("boring:build")
    refute Rake::Task.task_defined?("boringbuilder:build")
  end

  def test_builds_the_rails_application
    calls = []
    start = lambda do |arguments|
      calls << arguments
      0
    end

    with_environment("BORINGBUILDER_RUNTIME" => "docker", "BORINGBUILDER_OUTPUT" => "dist/app.tar.zst") do
      with_rails_root do
        with_cli_start(start) do
          Rake::Task["boring:build"].invoke
        end
      end
    end

    assert_equal [
      ["build", "--runtime", "docker", "--output", "dist/app.tar.zst", Pathname.pwd.to_s]
    ], calls
  end

  private

  def with_cli_start(replacement)
    singleton_class = BoringBuilder::CLI.singleton_class
    original = BoringBuilder::CLI.method(:start)
    singleton_class.send(:remove_method, :start)
    singleton_class.define_method(:start, replacement)
    yield
  ensure
    singleton_class.send(:remove_method, :start)
    singleton_class.define_method(:start, original)
  end

  def with_environment(values)
    previous = values.to_h { |name, _value| [name, ENV.fetch(name, nil)] }
    values.each { |name, value| ENV[name] = value }
    yield
  ensure
    previous.each { |name, value| value.nil? ? ENV.delete(name) : ENV[name] = value }
  end

  def with_rails_root
    return yield if Object.const_defined?(:Rails, false)

    rails = Module.new
    rails.define_singleton_method(:root) { Pathname.pwd }
    Object.const_set(:Rails, rails)
    yield
  ensure
    Object.send(:remove_const, :Rails) if rails
  end
end
