# frozen_string_literal: true

module BoringBuilder
  Result = Data.define(:format, :path, :reference, :runtime, :platform, :artifact_id, :artifact_name) do
    def initialize(format:, path:, reference:, runtime:, platform:, artifact_id: nil, artifact_name: nil)
      super
    end

    def exported?
      !path.nil?
    end

    def published?
      !reference.nil?
    end

    def artifact_published?
      !artifact_id.nil?
    end

    def to_h
      {
        format: format,
        path: path,
        reference: reference,
        runtime: runtime,
        platform: platform,
        artifact_id: artifact_id,
        artifact_name: artifact_name
      }
    end
  end
end
