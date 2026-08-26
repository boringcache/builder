# frozen_string_literal: true

require "open3"
require "tmpdir"

module BoringBuilder
  class Exporter
    PACKER_IMAGE = "alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce"

    attr_reader :project, :client, :container, :runtime, :exporters, :progress

    def initialize(project, client, container, runtime:, command_runner: nil, exporters: nil,
                   progress: BuildProgress.silent)
      @project = project
      @client = client
      @container = container
      @runtime = runtime
      @command_runner = command_runner || ->(*command) { Open3.capture3(*command) }
      @exporters = exporters || Exporters::Resolver.new(project, client).destinations
      @progress = progress
    end

    def call
      exporters.each(&:validate!)
      asset = build_asset unless exporters.empty?
      receipt = exporters.reduce(Exporters::Receipt.new) do |result, adapter|
        received = progress.step(export_step_name(adapter)) { adapter.export(asset) }
        merge_receipts(result, received)
      end
      reference = export_reference
      Result.new(
        format: configuration.format,
        path: receipt.path,
        reference: reference,
        runtime: runtime,
        platform: configuration.platform,
        artifact_id: receipt.artifact_id,
        artifact_name: receipt.artifact_name
      )
    rescue DaggerRuby::DaggerError => e
      raise ExportError, "Dagger could not export the build: #{e.message}"
    end

    private

    def configuration
      project.configuration
    end

    def merge_receipts(current, received)
      Exporters::Receipt.new(
        path: received.path || current.path,
        artifact_id: received.artifact_id || current.artifact_id,
        artifact_name: received.artifact_name || current.artifact_name
      )
    end

    def export_step_name(adapter)
      return "Publish artifact to BoringCache" if adapter.name == :boringcache

      "Export #{configuration.format.to_s.tr('_', '.')} artifact"
    end

    def build_asset
      filename = project.output_path.basename.to_s
      case configuration.format
      when :directory
        Exporters::Asset.new(kind: :directory, source: artifact_directory, filename: filename)
      when :tar
        Exporters::Asset.new(kind: :file, source: archive_file("artifact.tar", compression: nil), filename: filename)
      when :tar_zst
        Exporters::Asset.new(kind: :file, source: archive_file("artifact.tar.zst", compression: :zstd),
                             filename: filename)
      when :oci, :docker
        Exporters::Asset.new(kind: :file, source: container.as_tarball(media_types: media_types), filename: filename)
      end
    end

    def export_reference
      if configuration.publish
        progress.step("Publish container image") do
          container.publish(configuration.publish, media_types: media_types)
        end
      elsif configuration.load
        progress.step("Load container image") { load_image }
      end
    end

    def load_image
      return load_apple_image if runtime == :apple

      container.export_image(configuration.load, media_types: media_types)
      configuration.load
    end

    def load_apple_image
      Dir.mktmpdir("boringbuilder-load") do |directory|
        archive = File.join(directory, "image.oci.tar")
        container.export(archive, media_types: :OCIMediaTypes)
        source = load_apple_archive(archive)
        run_apple_command("container", "image", "tag", source, configuration.load)
      end
      configuration.load
    end

    def load_apple_archive(archive)
      output = run_apple_command("container", "image", "load", "--input", archive)
      source = output[/\b(?:untagged@)?sha256:[0-9a-f]{64}\b/]
      return source if source

      raise ExportError, "Apple Container loaded the image without returning a taggable digest"
    end

    def run_apple_command(*command)
      stdout, stderr, status = @command_runner.call(*command)
      output = [stdout, stderr].map(&:strip).reject(&:empty?).join("\n")
      return output if status.success?

      raise ExportError, "#{command.first(3).join(' ')} failed: #{output}"
    end

    def artifact_directory
      @artifact_directory ||= project.artifact.export_directory(client, container)
    end

    def archive_file(filename, compression:)
      packer = client.container.from(PACKER_IMAGE).with_exec(%w[apk add --no-cache tar zstd])
      packer = packer.with_mounted_directory("/artifact", artifact_directory)
      tar_path = compression == :zstd ? "/artifact.tar" : "/#{filename}"
      packer = packer.with_exec(
        [
          "tar", "--sort=name", "--mtime=@0", "--owner=0", "--group=0", "--numeric-owner",
          "--pax-option=delete=atime,delete=ctime", "-C", "/artifact", "-cf", tar_path, "."
        ]
      )
      packer = packer.with_exec(["zstd", "-q", "-T1", tar_path, "-o", "/#{filename}"]) if compression == :zstd
      packer.file("/#{filename}")
    end

    def media_types
      configuration.format == :docker ? :DockerMediaTypes : :OCIMediaTypes
    end
  end
end
