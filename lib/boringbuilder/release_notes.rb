# frozen_string_literal: true

module BoringBuilder
  class ReleaseNotes
    def self.read(path:, version:)
      sections = File.read(path).lines
      heading = /^## #{Regexp.escape(version)} - \d{4}-\d{2}-\d{2}\s*$/
      start = sections.index { |line| line.match?(heading) }
      raise ArgumentError, "CHANGELOG.md has no release notes for #{version}" unless start

      finish = sections.each_index.find { |index| index > start && sections[index].start_with?("## ") }
      notes = sections[(start + 1)...finish].join.strip
      raise ArgumentError, "CHANGELOG.md release notes for #{version} are empty" if notes.empty?

      notes
    end
  end
end
