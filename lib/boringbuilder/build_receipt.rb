# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "pathname"
require "tempfile"
require "time"

module BoringBuilder
  class BuildReceipt
    RELATIVE_PATH = "tmp/builds/boringbuilder.json"

    def self.write(root:, result:, clock: Time, command_runner: Open3.method(:capture3))
      return unless result.exported? || result.artifact_published? || result.published?

      root = Pathname.new(root).expand_path
      path = root.join(RELATIVE_PATH)
      FileUtils.mkdir_p(path.dirname)
      document = {
        schema_version: 1,
        created_at: clock.now.utc.iso8601,
        source_revision: source_revision(root, command_runner),
        result: result.to_h
      }

      Tempfile.create(["boringbuilder", ".json"], path.dirname) do |file|
        file.write(JSON.pretty_generate(document))
        file.write("\n")
        file.flush
        file.fsync
        File.rename(file.path, path)
      end

      path
    end

    def self.source_revision(root, command_runner)
      stdout, _stderr, status = command_runner.call("git", "-C", root.to_s, "rev-parse", "HEAD")
      status.success? ? stdout.strip : nil
    rescue Errno::ENOENT
      nil
    end

    private_class_method :source_revision
  end
end
