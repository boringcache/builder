# frozen_string_literal: true

require "pathname"

module BoringBuilder
  class Artifact
    Entry = Data.define(:origin, :kind, :source, :destination) do
      def to_h
        { origin: origin, kind: kind, source: source.to_s, destination: destination }
      end
    end

    attr_reader :root, :entries

    def initialize(root: Dir.pwd)
      @root = Pathname.new(root).expand_path
      @entries = []
    end

    def directory(source, at: source)
      add(:container, :directory, source, at)
    end

    def file(source, at: source)
      add(:container, :file, source, at)
    end

    def host_path(source, at:)
      path = Pathname.new(source).expand_path(root)
      kind = path.file? ? :file : :directory
      raise ConfigurationError, "Host artifact path does not exist: #{path}" unless path.exist?

      add(:host, kind, path, at)
    end

    def empty?
      entries.empty?
    end

    def full_container_root?
      entries.one? && entries.first.origin == :container && entries.first.kind == :directory &&
        entries.first.source == "/" && entries.first.destination == "/"
    end

    def export_directory(client, container)
      return container.directory("/") if full_container_root?

      entries.reduce(client.directory) do |directory, entry|
        destination = entry.destination.delete_prefix("/")
        source = source_for(entry, client, container)

        if entry.kind == :file
          directory.with_file(destination, source)
        else
          directory.with_directory(destination, source)
        end
      end
    end

    def to_a
      entries.map(&:to_h)
    end

    private

    def add(origin, kind, source, destination)
      source = normalize_container_path(source, "source") if origin == :container
      destination = normalize_container_path(destination, "destination")
      entries << Entry.new(origin: origin, kind: kind, source: source, destination: destination)
      self
    end

    def normalize_container_path(path, label)
      value = path.to_s
      clean = Pathname.new(value).cleanpath.to_s
      return clean if value.start_with?("/") && clean == value

      raise ConfigurationError, "Artifact #{label} must be a clean absolute path: #{path.inspect}"
    end

    def source_for(entry, client, container)
      source = entry.source.to_s
      owner = entry.origin == :container ? container : client.host
      entry.kind == :file ? owner.file(source) : owner.directory(source)
    end
  end
end
