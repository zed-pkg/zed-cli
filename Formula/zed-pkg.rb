# Homebrew formula for the zed-pkg CLI. Published via the zed-pkg/homebrew-tap
# repo; kept here as the source of truth.
#
#   brew tap zed-pkg/tap
#   brew install zed-pkg
class ZedPkg < Formula
  desc "Universal package manager backed by the VCS hosts you already use"
  homepage "https://zpkg.tech"
  url "https://github.com/zed-pkg/zed-cli/archive/refs/tags/v0.1.0.tar.gz"
  # TODO: fill in after cutting the v0.1.0 release tag:
  #   curl -L <url> | shasum -a 256
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"

  depends_on "rust" => :build

  # The Zed editor's formula also installs a `zed` binary.
  conflicts_with "zed", because: "both install a `zed` binary"

  def install
    # zed-cli depends on zed-interfaces by path (../zed-interfaces); release
    # tarballs vendor it via the release workflow. For source builds from a
    # side-by-side checkout, plain `cargo install --path .` works.
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "zed", shell_output("#{bin}/zed --help")
  end
end
