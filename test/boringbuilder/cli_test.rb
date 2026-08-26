# frozen_string_literal: true

require "test_helper"

class CLITest < Minitest::Test
  include ProjectFixture

  FakeBuilder = Struct.new(:configuration, :progress, keyword_init: true) do
    def initialize(configuration, progress:)
      super(configuration: configuration, progress: progress)
    end

    def build
      progress.step("Build application") { true }
      BoringBuilder::Result.new(
        format: configuration.format,
        path: configuration.output&.to_s,
        reference: configuration.publish,
        runtime: :apple,
        platform: configuration.platform,
        artifact_id: configuration.artifact_name && "art_0123456789abcdef01234567",
        artifact_name: configuration.artifact_name
      )
    end
  end

  def test_prints_version
    out = StringIO.new

    status = BoringBuilder::CLI.start(["version"], out: out, err: StringIO.new)

    assert_equal 0, status
    assert_equal "#{BoringBuilder::VERSION}\n", out.string
  end

  def test_prints_a_dry_run_plan
    root = build_project
    out = StringIO.new

    status = BoringBuilder::CLI.start(["build", "--dry-run", root.to_s], out: out, err: StringIO.new)

    assert_equal 0, status
    plan = JSON.parse(out.string)

    assert_equal "rails", plan.fetch("strategy")
    assert_equal "tar_zst", plan.fetch("format")
  end

  def test_build_accepts_runtime_output_and_registry_options
    root = build_project
    out = StringIO.new

    status = BoringBuilder::CLI.start(
      ["build", "--runtime", "apple", "--format", "oci", "--push", "example/app:v1", root.to_s],
      out: out,
      err: StringIO.new,
      builder_class: FakeBuilder
    )

    assert_equal 0, status
    assert_match(/Built with Dagger on apple/, out.string)
    assert_match(%r{Image: example/app:v1}, out.string)
  end

  def test_describes_the_build_inside_a_dagger_session
    root = build_project(name: "Friendly Rails")
    out = StringIO.new
    environment = { "DAGGER_SESSION_PORT" => "1234", "DAGGER_SESSION_TOKEN" => "secret" }

    status = BoringBuilder::CLI.start(
      ["build", "--runtime", "docker", "--format", "oci", root.to_s],
      out: out,
      err: StringIO.new,
      builder_class: FakeBuilder,
      environment: environment
    )

    assert_equal 0, status
    assert_includes out.string, "Building friendly-rails with Dagger"
    assert_includes out.string, "rails · docker runtime · local cache · oci output"
    assert_includes out.string, "#1 Build application"
    assert_match(/#1 DONE \d+\.\d+s/, out.string)
  end

  def test_prints_the_shared_artifact_receipt_as_json
    root = build_project
    out = StringIO.new

    status = BoringBuilder::CLI.start(
      ["build", "--json", "--artifact-name", "web-release", root.to_s],
      out: out,
      err: StringIO.new,
      builder_class: FakeBuilder
    )

    assert_equal 0, status
    result = JSON.parse(out.string)

    assert_equal "art_0123456789abcdef01234567", result.fetch("artifact_id")
    assert_equal "web-release", result.fetch("artifact_name")
  end

  def test_dry_run_describes_a_custom_artifact_layout
    root = build_project
    release_metadata = root.parent.join("release-metadata")
    FileUtils.mkdir_p(release_metadata)
    out = StringIO.new

    status = BoringBuilder::CLI.start(
      [
        "build", "--dry-run",
        "--path", "/rails=/opt/my-app/current",
        "--file", "/usr/local/bin/mise=/usr/local/bin/mise",
        "--host-path", "#{release_metadata}=/opt/my-app/release-metadata",
        root.to_s
      ],
      out: out,
      err: StringIO.new
    )

    assert_equal 0, status
    artifact = JSON.parse(out.string).fetch("artifact")
    origins = artifact.map { |entry| entry.fetch("origin") }

    assert_equal %w[container container host], origins
    assert_equal "/opt/my-app/current", artifact.first.fetch("destination")
  end

  def test_auto_loads_the_project_recipe_and_allows_cli_overrides
    root = build_project
    File.write(root.join("config/boringbuilder.rb"), <<~RUBY)
      BoringBuilder.configure do |config|
        config.runtime = :apple
        config.format = :tar_zst
        config.artifact.directory("/rails", at: "/opt/my-app/current")
      end
    RUBY
    out = StringIO.new

    status = BoringBuilder::CLI.start(
      ["build", "--dry-run", "--runtime", "docker", "--format", "oci", root.to_s],
      out: out,
      err: StringIO.new
    )

    assert_equal 0, status
    plan = JSON.parse(out.string)

    assert_equal "docker", plan.fetch("runtime")
    assert_equal "oci", plan.fetch("format")
    assert_equal "/opt/my-app/current", plan.fetch("artifact").first.fetch("destination")
  end

  def test_help_does_not_build
    out = StringIO.new

    status = BoringBuilder::CLI.start(["build", "--help"], out: out, err: StringIO.new)

    assert_equal 0, status
    assert_match(/Usage: boringbuilder build/, out.string)
  end

  def test_doctor_help_does_not_probe_a_runtime
    out = StringIO.new

    status = BoringBuilder::CLI.start(["doctor", "--help"], out: out, err: StringIO.new)

    assert_equal 0, status
    assert_match(/Usage: boringbuilder doctor/, out.string)
  end

  def test_init_generates_a_selected_application_recipe
    root = Pathname.new(Dir.mktmpdir("boringbuilder-cli-init"))
    (@fixture_directories ||= []) << root
    out = StringIO.new

    status = BoringBuilder::CLI.start(
      ["init", "--template", "rust", root.to_s],
      out: out,
      err: StringIO.new
    )

    assert_equal 0, status
    assert_equal "Created config/boringbuilder.rb (rust)\n", out.string
    assert_includes root.join("config/boringbuilder.rb").read, "%w[cargo build --release]"
  end

  def test_reports_errors_to_the_injected_stream
    error_output = StringIO.new

    status = BoringBuilder::CLI.start(["unknown"], out: StringIO.new, err: error_output)

    assert_equal 1, status
    assert_match(/Unknown command/, error_output.string)
  end
end
