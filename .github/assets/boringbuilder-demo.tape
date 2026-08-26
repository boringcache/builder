# Run from the repository root with: vhs .github/assets/boringbuilder-demo.tape

Output .github/assets/boringbuilder-demo.gif

Require bundle
Require dagger
Require gem

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 760
Set Padding 24
Set Framerate 15
Set PlaybackSpeed 5
Set TypingSpeed 36ms
Set Theme "Catppuccin Mocha"
Set WindowBar "Colorful"
Set WindowBarSize 34

Hide
Type "demo_root=/private/tmp/boringbuilder-demo" Enter
Type "gem_root=/private/tmp/boringbuilder-demo-gems" Enter
Type "rm -rf $demo_root" Enter
Type "mkdir -p $demo_root $gem_root" Enter
Type "cp -R test/fixtures/custom_node_app/. $demo_root" Enter
Type "rm -rf $demo_root/config $demo_root/dist" Enter
Type "bundle exec rake build >/dev/null" Enter
Type "GEM_HOME=$gem_root GEM_PATH=$gem_root gem install --ignore-dependencies --env-shebang --no-document --bindir $gem_root/bin pkg/boringbuilder-*.gem >/dev/null" Enter
Type "export GEM_HOME=$gem_root GEM_PATH=$gem_root PATH=$gem_root/bin:$PATH" Enter
Type "export DAGGER_SILENT=true DAGGER_NO_NAG=1" Enter
Type "cd $demo_root" Enter
Type "export PS1='$ '" Enter
Type "reset" Enter
Sleep 2s
Show

Type "boringbuilder init" Enter
Sleep 2s
Type "sed -n '1,24p' config/boringbuilder.rb" Enter
Sleep 2s
Type "boringbuilder build --runtime docker --format directory --output artifact" Enter
Sleep 120s
Sleep 2s
Type "find artifact -type f" Enter
Sleep 4s
