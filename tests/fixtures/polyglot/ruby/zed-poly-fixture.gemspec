# Files are globbed rather than read from `git ls-files`: the published slice
# is a re-rooted tarball with no VCS metadata, so a git-based file list would
# build an empty gem.
Gem::Specification.new do |s|
  s.name        = "zed-poly-fixture"
  s.version     = "0.2.0"
  s.summary     = "Polyglot publish fixture (ruby slice)"
  s.description = "Polyglot publish fixture (ruby slice)"
  s.authors     = ["zed-pkg"]
  s.license     = "MIT"
  s.homepage    = "https://github.com/zed-pkg/zed-cli"
  s.files       = Dir.glob("lib/**/*.rb") + Dir.glob("LICENSE*")
  s.require_paths = ["lib"]
  s.required_ruby_version = ">= 3.0"
end
