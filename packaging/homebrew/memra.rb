class Memra < Formula
  desc "Durable AI memory for MCP-compatible coding agents"
  homepage "https://github.com/pokibao/memra"
  version "6.0.1"

  # Supported targets match rust-release.yml. Intel Mac and Linux ARM64 are
  # blocked by ort-sys prebuilt binary availability; see T9 follow-up.
  on_macos do
    on_arm do
      url "https://github.com/pokibao/memra/releases/download/v#{version}/memra-aarch64-apple-darwin.tar.gz"
      sha256 "a9a6c0297b29bf12f0f710f7555cab33c15e883864291939ad8df48a3362f6d6"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/pokibao/memra/releases/download/v#{version}/memra-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "2461cb7c2fb2ffb34d5a3f4a0d6ced92cc8a5a9107d4bb2a4fa2775044ae6017"
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
