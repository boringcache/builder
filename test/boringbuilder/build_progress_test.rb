# frozen_string_literal: true

require "test_helper"

class BuildProgressTest < Minitest::Test
  def test_prints_numbered_steps_and_returns_the_result
    output = StringIO.new
    progress = BoringBuilder::BuildProgress.new(out: output)

    result = progress.step("Install dependencies") { :finished }

    assert_equal :finished, result
    assert_includes output.string, "#1 Install dependencies\n"
    assert_match(/#1 DONE \d+\.\d+s\n/, output.string)
  end

  def test_numbers_steps_in_execution_order
    output = StringIO.new
    progress = BoringBuilder::BuildProgress.new(out: output)

    progress.step("Prepare Mise toolchain") { true }
    progress.step("Build application") { true }

    assert_operator output.string.index("#1 Prepare Mise toolchain"), :<, output.string.index("#2 Build application")
  end

  def test_marks_a_failed_step_and_preserves_the_error
    output = StringIO.new
    progress = BoringBuilder::BuildProgress.new(out: output)

    error = assert_raises(RuntimeError) do
      progress.step("Build application") { raise "broken build" }
    end

    assert_equal "broken build", error.message
    assert_match(/#1 ERROR \d+\.\d+s\n/, output.string)
  end

  def test_indents_command_output
    output = StringIO.new
    progress = BoringBuilder::BuildProgress.new(out: output)

    progress.step("Build application") { progress.write("compiled\nready") }

    assert_includes output.string, "    compiled\n    ready\n"
  end

  def test_silent_progress_executes_without_output
    output = StringIO.new
    progress = BoringBuilder::BuildProgress.new(out: output, enabled: false)

    result = progress.step("Build application") { :finished }

    assert_equal :finished, result
    assert_empty output.string
  end
end
