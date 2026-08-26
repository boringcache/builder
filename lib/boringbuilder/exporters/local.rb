# frozen_string_literal: true

require "fileutils"

module BoringBuilder
  module Exporters
    class Local
      attr_reader :project

      def initialize(project)
        @project = project
      end

      def name = :local

      def validate! = self

      def export(asset)
        output = project.output_path
        FileUtils.mkdir_p(output.dirname)
        options = asset.kind == :directory ? { wipe: true } : {}
        asset.source.export(output.to_s, **options)
        Receipt.new(path: output.to_s)
      end
    end
  end
end
