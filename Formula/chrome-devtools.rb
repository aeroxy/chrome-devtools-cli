class ChromeDevtools < Formula
  desc "Chrome DevTools Protocol CLI — auto-connects to existing Chrome"
  homepage "https://github.com/aeroxy/chrome-devtools-cli"
  version "0.1.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/chrome-devtools-cli/releases/download/0.1.0/chrome-devtools-macos-arm64.zip"
      sha256 "b3b179dc55ebcaa6994294fab0d3ea0bfe3b97e3686eb61bbd59fb6256e31d2c"
    end
  end

  def install
    bin.install "chrome-devtools"
  end

  test do
    assert_match "chrome-devtools", shell_output("#{bin}/chrome-devtools --help")
  end
end
