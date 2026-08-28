# frozen_string_literal: true

require "test_helper"

class BuildReceiptTest < Minitest::Test
  include ProjectFixture

  def test_records_the_exact_build_result_and_source_revision
    root = build_project
    result = BoringBuilder::Result.new(
      format: :tar_zst,
      path: nil,
      reference: nil,
      runtime: :apple,
      platform: "linux/arm64",
      artifact_id: "art_0123456789abcdef01234567",
      artifact_name: "example-native-tar-zst"
    )
    status = Struct.new(:success?).new(true)
    runner = ->(*_command) { ["abc123\n", "", status] }
    clock = Struct.new(:now).new(Time.utc(2026, 8, 28, 15, 30))

    path = BoringBuilder::BuildReceipt.write(root: root, result: result, clock: clock, command_runner: runner)
    receipt = JSON.parse(path.read)

    assert_equal root.join("tmp/builds/boringbuilder.json"), path
    assert_equal 1, receipt.fetch("schema_version")
    assert_equal "2026-08-28T15:30:00Z", receipt.fetch("created_at")
    assert_equal "abc123", receipt.fetch("source_revision")
    assert_equal "art_0123456789abcdef01234567", receipt.dig("result", "artifact_id")
    assert_equal "example-native-tar-zst", receipt.dig("result", "artifact_name")
  end

  def test_does_not_write_a_receipt_when_nothing_was_exported
    root = build_project
    result = BoringBuilder::Result.new(
      format: :tar_zst,
      path: nil,
      reference: nil,
      runtime: :docker,
      platform: "linux/amd64"
    )

    assert_nil BoringBuilder::BuildReceipt.write(root: root, result: result)
    refute_predicate root.join("tmp/builds/boringbuilder.json"), :exist?
  end
end
