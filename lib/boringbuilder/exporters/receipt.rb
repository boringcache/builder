# frozen_string_literal: true

module BoringBuilder
  module Exporters
    Receipt = Data.define(:path, :artifact_id, :artifact_name) do
      def initialize(path: nil, artifact_id: nil, artifact_name: nil)
        super
      end
    end
  end
end
