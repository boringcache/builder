# frozen_string_literal: true

require "test_helper"
require "boringbuilder/release_notes"

class ReleaseNotesTest < Minitest::Test
  def test_reads_one_human_changelog_section
    changelog = Tempfile.new("changelog")
    changelog.write(<<~MARKDOWN)
      # Changelog

      ## 1.2.3 - 2026-08-28

      - Build receipts connect builds to deploys.

      ## 1.2.2 - 2026-08-27

      - Earlier work.
    MARKDOWN
    changelog.close

    notes = BoringBuilder::ReleaseNotes.read(path: changelog.path, version: "1.2.3")

    assert_equal "- Build receipts connect builds to deploys.", notes
  ensure
    changelog&.unlink
  end

  def test_rejects_a_missing_release
    error = assert_raises(ArgumentError) do
      BoringBuilder::ReleaseNotes.read(path: "CHANGELOG.md", version: "9.9.9")
    end

    assert_equal "CHANGELOG.md has no release notes for 9.9.9", error.message
  end
end
