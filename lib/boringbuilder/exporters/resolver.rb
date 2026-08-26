# frozen_string_literal: true

module BoringBuilder
  module Exporters
    class Resolver
      ADAPTERS = {
        local: Local,
        boringcache: BoringCache
      }.freeze

      attr_reader :project, :client, :cache

      def initialize(project, client, environment: ENV)
        @project = project
        @client = client
        @environment = environment
        @cache = ::BoringBuilder::BoringCache.new(project, client, environment: environment)
      end

      def destinations
        names = [primary_name].compact
        names << :local if configuration.output && !names.include?(:local)
        names.map { |name| adapter(name) }
      end

      def plan
        destinations.map(&:name)
      end

      def validate!
        destinations.each(&:validate!)
        self
      end

      private

      def configuration
        project.configuration
      end

      def primary_name
        return configuration.exporter unless configuration.exporter == :auto
        return :boringcache if cache.artifact_publishable?
        return if configuration.publish || configuration.load

        :local
      end

      def adapter(name)
        adapter = ADAPTERS.fetch(name)
        return adapter.new(project) if name == :local

        adapter.new(project, client, environment: @environment)
      end
    end
  end
end
