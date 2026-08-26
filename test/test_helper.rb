# frozen_string_literal: true

require "simplecov"

SimpleCov.start do
  enable_coverage :branch
  skip "/test/"
end

require "fileutils"
require "minitest/autorun"
require "stringio"
require "tmpdir"
require "boringbuilder"

module ProjectFixture
  def build_project(name: "my_app")
    fixture_directory = Pathname.new(Dir.mktmpdir("boringbuilder-test"))
    root = fixture_directory.join(name)
    FileUtils.mkdir_p(root.join("config"))
    FileUtils.mkdir_p(root.join("app/assets"))
    FileUtils.mkdir_p(root.join("bin"))
    File.write(root.join("Gemfile"), "source \"https://rubygems.org\"\ngem \"rails\"\n")
    File.write(root.join("Gemfile.lock"), <<~LOCK)
      GEM
        specs:
          bootsnap (1.18.6)

      RUBY VERSION
         ruby 3.4.9p0
    LOCK
    File.write(root.join("config/application.rb"), "class Application < Rails::Application; end\n")
    File.write(root.join("bin/rails"), "#!/usr/bin/env ruby\n")
    @fixture_directories ||= []
    @fixture_directories << fixture_directory
    root
  end

  def teardown
    Array(@fixture_directories).each { |directory| FileUtils.remove_entry(directory) if directory.exist? }
    super
  end
end

class RecordingNode
  attr_reader :calls

  def initialize(calls, stdout: nil, stderr: nil)
    @calls = calls
    @stdout = stdout
    @stderr = stderr
  end

  def method_missing(name, *arguments, **options)
    calls << [name, arguments, options]
    case name
    when :publish then "registry.example/app@sha256:123"
    when :export_image, :export then arguments.first
    when :stdout then @stdout || self
    when :stderr then @stderr || self
    else self
    end
  end

  def respond_to_missing?(_name, _include_private = false)
    true
  end
end

class RecordingClient
  attr_reader :calls

  def initialize(stdout: nil, stderr: nil)
    @calls = []
    @node = RecordingNode.new(@calls, stdout: stdout, stderr: stderr)
  end

  def container(options = {})
    calls << [:container, [options], {}]
    @node
  end

  def directory
    calls << [:directory, [], {}]
    @node
  end

  def host
    calls << [:host, [], {}]
    @node
  end

  def cache_volume(name)
    calls << [:cache_volume, [name], {}]
    @node
  end

  def set_secret(name, value)
    calls << [:set_secret, [name, value], {}]
    @node
  end
end
