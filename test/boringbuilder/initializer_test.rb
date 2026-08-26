# frozen_string_literal: true

require "test_helper"

class InitializerTest < Minitest::Test
  def teardown
    Array(@roots).each { |root| FileUtils.remove_entry(root) if root.exist? }
    super
  end

  def test_detects_node_and_generates_a_cache_aware_build_recipe
    root = project("package.json" => '{"scripts":{"build":"node build.js"}}', "package-lock.json" => "{}")
    initializer = BoringBuilder::Initializer.new(root)

    path = initializer.call
    contents = path.read

    assert_equal :node, initializer.template_name
    assert_includes contents, 'config.artifact.directory("/app/dist", at: "/app")'
    assert_includes contents, 'pipeline.mise(tools: { node: "24" }, workdir: "/app")'
    assert_includes contents, "%w[npm ci]"
    assert_includes contents, 'cache: "npm-downloads"'
    assert_includes contents, 'at: "/root/.npm/_cacache"'
    assert_includes contents, 'name: "Install dependencies"'
    assert_includes contents, 'pipeline.exec(app, %w[npm run build], name: "Build application")'
    RubyVM::InstructionSequence.compile(contents)
  end

  def test_generates_rust_go_ruby_and_generic_recipes
    {
      "Cargo.toml" => ["rust", 'pipeline.mise(tools: { rust: "stable" }'],
      "go.mod" => ["go", 'pipeline.mise(tools: { go: "1" }'],
      "Gemfile" => ["ruby", "built-in Mise-powered pipeline"]
    }.each do |manifest, (name, expected)|
      root = project(manifest => "")
      path = BoringBuilder::Initializer.new(root).call

      assert_includes path.read, expected
      assert_equal name, BoringBuilder::Initializer.new(root).template_name.to_s
      RubyVM::InstructionSequence.compile(path.read)
    end

    path = BoringBuilder::Initializer.new(project).call

    assert_includes path.read, "pipeline.mise("
    assert_includes path.read, '# "python" => "3"'
    RubyVM::InstructionSequence.compile(path.read)
  end

  def test_refuses_to_replace_a_recipe_without_force
    root = project
    initializer = BoringBuilder::Initializer.new(root, template: :generic)
    initializer.call

    error = assert_raises(BoringBuilder::ConfigurationError) { initializer.call }

    assert_match(/pass --force/, error.message)
    assert_equal initializer.call(force: true), root.join("config/boringbuilder.rb")
  end

  def test_handles_a_package_file_without_a_scripts_object
    root = project("package.json" => "[]")

    contents = BoringBuilder::Initializer.new(root).call.read

    assert_includes contents, "%w[npm install]"
    refute_includes contents, "%w[npm run build]"
  end

  private

  def project(files = {})
    root = Pathname.new(Dir.mktmpdir("boringbuilder-init"))
    files.each do |name, contents|
      path = root.join(name)
      FileUtils.mkdir_p(path.dirname)
      path.write(contents)
    end
    @roots ||= []
    @roots << root
    root
  end
end
