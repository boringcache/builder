# frozen_string_literal: true

require "test_helper"

class ExporterTest < Minitest::Test
  include ProjectFixture

  def test_exports_selected_paths_as_a_zstandard_tarball
    root = build_project
    output = root.join("out/app.tar.zst")
    configuration = BoringBuilder::Configuration.new(
      root: root,
      paths: ["/rails", "/usr/local/bundle"],
      output: output,
      format: :tar_zst
    )
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)

    result = BoringBuilder::Exporter.new(project, client, container, runtime: :apple).call

    assert_equal output.to_s, result.path
    assert_equal :tar_zst, result.format
    assert_equal :apple, result.runtime
    assert_includes client.calls, [:from, [BoringBuilder::Exporter::PACKER_IMAGE], {}]
    assert_includes client.calls, [:directory, [], {}]
    assert_includes client.calls, [:with_directory, ["rails", container], {}]
    assert_includes client.calls,
                    [:with_exec,
                     [["tar", "--sort=name", "--mtime=@0", "--owner=0", "--group=0", "--numeric-owner",
                       "--pax-option=delete=atime,delete=ctime", "-C", "/artifact", "-cf", "/artifact.tar", "."]],
                     {}]
    zstd_call = client.calls.find do |name, arguments, _options|
      name == :with_exec && arguments.first.first == "zstd"
    end

    refute_nil zstd_call
    assert_includes zstd_call[1].first, "-T0"
  end

  def test_describes_the_real_archive_steps
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, format: :tar_zst)
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    progress_output = StringIO.new

    BoringBuilder::Exporter.new(
      project,
      client,
      RecordingNode.new(client.calls),
      runtime: :docker,
      progress: DaggerRuby::Progress.new(out: progress_output)
    ).call

    assert_includes progress_output.string, "[export] Assemble artifact filesystem"
    assert_includes progress_output.string, "[export] RUN tar -cf /artifact.tar ."
    assert_includes progress_output.string, "[export] RUN zstd -T0 /artifact.tar"
    assert_includes progress_output.string, "[export] Write tar.zst artifact"
  end

  def test_assembles_remapped_container_and_host_content
    root = build_project
    release_metadata = root.parent.join("release-metadata")
    FileUtils.mkdir_p(release_metadata)
    File.write(release_metadata.join("revision.txt"), "abc123\n")
    configuration = BoringBuilder::Configuration.new(
      root: root,
      paths: ["/rails=/opt/my-app/current"],
      files: ["/usr/local/bin/mise=/usr/local/bin/mise"],
      host_paths: ["#{release_metadata}=/opt/my-app/release-metadata"],
      output: root.join("out/app.tar.zst"),
      format: :tar_zst
    )
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)

    BoringBuilder::Exporter.new(project, client, container, runtime: :docker).call

    assert_includes client.calls, [:with_directory, ["opt/my-app/current", container], {}]
    assert_includes client.calls, [:with_file, ["usr/local/bin/mise", container], {}]
    assert_includes client.calls, [:directory, [release_metadata.to_s], {}]
    assert(client.calls.any? do |name, arguments, _options|
      name == :with_directory && arguments.first == "opt/my-app/release-metadata"
    end)
  end

  def test_exports_an_oci_archive
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, format: :oci)
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)

    result = BoringBuilder::Exporter.new(project, client, container, runtime: :docker).call

    assert_equal project.output_path.to_s, result.path
    assert_includes client.calls, [:as_tarball, [], { media_types: :OCIMediaTypes }]
    assert_includes client.calls, [:export, [project.output_path.to_s], {}]
  end

  def test_shared_artifact_is_the_default_destination_when_available
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root)
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)
    exporter = Struct.new(:receipt) do
      def name = :boringcache
      def validate! = self
      def export(_asset) = receipt
    end.new(
      BoringBuilder::Exporters::Receipt.new(
        artifact_id: "art_0123456789abcdef01234567",
        artifact_name: "my-app-native-tar-zst"
      )
    )

    result = BoringBuilder::Exporter.new(
      project,
      client,
      container,
      runtime: :docker,
      exporters: [exporter]
    ).call

    assert_nil result.path
    assert_equal "art_0123456789abcdef01234567", result.artifact_id
    assert_equal "my-app-native-tar-zst", result.artifact_name
  end

  def test_push_skips_the_implicit_local_export
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, publish: "registry.example/app:latest")
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)

    result = BoringBuilder::Exporter.new(project, client, container, runtime: :docker).call

    assert_nil result.path
    assert_equal "registry.example/app@sha256:123", result.reference
    assert_includes client.calls, [:publish, ["registry.example/app:latest"], { media_types: :OCIMediaTypes }]
  end

  def test_load_uses_the_selected_image_media_type
    root = build_project
    configuration = BoringBuilder::Configuration.new(
      root: root,
      format: :docker,
      load: "my-app:latest"
    )
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)

    result = BoringBuilder::Exporter.new(project, client, container, runtime: :docker).call

    assert_nil result.path
    assert_equal "my-app:latest", result.reference
    assert_includes client.calls, [:export_image, ["my-app:latest"], { media_types: :DockerMediaTypes }]
  end

  def test_load_works_around_the_apple_dagger_tagging_bug
    root = build_project
    configuration = BoringBuilder::Configuration.new(root: root, load: "my-app:latest")
    project = BoringBuilder::Project.new(configuration).validate!
    client = RecordingClient.new
    container = RecordingNode.new(client.calls)
    commands = []
    status = Struct.new(:success?).new(true)
    runner = lambda do |*command|
      commands << command
      output = command.include?("load") ? "untagged@sha256:#{'a' * 64}\n" : ""
      [output, "", status]
    end

    result = BoringBuilder::Exporter.new(
      project,
      client,
      container,
      runtime: :apple,
      command_runner: runner
    ).call

    assert_equal "my-app:latest", result.reference
    assert_includes commands, ["container", "image", "tag", "untagged@sha256:#{'a' * 64}", "my-app:latest"]
    assert(
      client.calls.any? do |name, _arguments, options|
        name == :export && options == { media_types: :OCIMediaTypes }
      end
    )
  end
end
