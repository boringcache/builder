# frozen_string_literal: true

require "json"

module BoringBuilder
  module Exporters
    class BoringCache
      RECEIPT_PATH = "/tmp/boringbuilder-artifact.json"

      attr_reader :project, :client, :cache

      def initialize(project, client, environment: ENV)
        @project = project
        @client = client
        @cache = ::BoringBuilder::BoringCache.new(project, client, environment: environment)
      end

      def name = :boringcache

      def validate!
        if configuration.artifact_name&.strip == ""
          raise ConfigurationError, "BoringCache Artifact name cannot be empty"
        end
        raise ConfigurationError, "The boringcache exporter requires BORINGCACHE_SAVE_TOKEN" unless cache.save_token?
        return self if cache.workspace_configured?

        raise ConfigurationError,
              "The boringcache exporter requires a workspace environment variable or .boringcache.toml"
      end

      def export(asset)
        validate!
        publisher = client.container.from(cache.cli_image)
        publisher = cache.prepare_artifact_publisher(publisher)
        path, publisher = mount_asset(publisher, asset)
        publisher = publisher.with_exec(quiet_publish_command(path, asset.kind))
        output = publisher.file(RECEIPT_PATH).contents
        artifact = JSON.parse(output).fetch("artifact")
        validate_artifact!(artifact)
        Receipt.new(artifact_id: artifact.fetch("id"), artifact_name: artifact.fetch("name"))
      rescue JSON::ParserError, KeyError => e
        raise ExportError, "BoringCache Artifact returned an invalid receipt: #{e.message}"
      end

      private

      def configuration
        project.configuration
      end

      def artifact_name
        configuration.artifact_name || [project.app_name, platform_name, format_name].join("-")
      end

      def platform_name
        (configuration.platform || "native").tr("/", "-")
      end

      def format_name
        configuration.format.to_s.tr("_", "-")
      end

      def mount_asset(publisher, asset)
        if asset.kind == :directory
          ["/artifact", publisher.with_mounted_directory("/artifact", asset.source)]
        else
          path = "/artifact/#{asset.filename}"
          [path, publisher.with_file(path, asset.source)]
        end
      end

      def publish_command(path, kind)
        command = ["boringcache", "artifact", "push", path, "--name", artifact_name,
                   "--include-hidden", "--json"]
        command.push("--compression", "none") unless kind == :directory
        command
      end

      def quiet_publish_command(path, kind)
        ["sh", "-c", "exec \"$@\" > #{RECEIPT_PATH}", "boringbuilder-artifact", *publish_command(path, kind)]
      end

      def validate_artifact!(artifact)
        return if artifact.fetch("id", "").start_with?("art_") && artifact.fetch("status", nil) == "ready"

        raise ExportError, "BoringCache Artifact did not return a ready artifact"
      end
    end
  end
end
