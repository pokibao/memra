class MemraATNext < Formula
  desc "Durable AI memory for MCP-compatible coding agents (next channel)"
  homepage "https://github.com/pokibao/memra"
  version "__VERSION__"

  # Supported targets match rust-release.yml. Intel Mac and Linux ARM64 are
  # blocked by ort-sys prebuilt binary availability; see T9 follow-up.
  on_macos do
    on_arm do
      url "https://github.com/pokibao/memra/releases/download/v#{version}/memra-aarch64-apple-darwin.tar.gz"
      sha256 "__MACOS_ARM64_SHA__"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/pokibao/memra/releases/download/v#{version}/memra-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "__LINUX_X64_SHA__"
    end
  end

  def install
    bin.install "memra"
    bin.install "memra-embed-worker"
  end

  test do
    assert_match "Memra", shell_output("#{bin}/memra --help")
  end
end
