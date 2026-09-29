class ChromeDevtools < Formula
  desc "Chrome DevTools Protocol CLI — auto-connects to existing Chrome"
  homepage "https://github.com/aeroxy/chrome-devtools-cli"
  version "2.1.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/chrome-devtools-cli/releases/download/2.1.0/chrome-devtools-macos-arm64.zip"
      sha256 "b51fce86ce1597eb4e1fae5f585c217a7d018592b5cd307ed4b7a30c2c9e1380"
    end
  end

  def install
    bin.install "chrome-devtools"
  end

  test do
    assert_match "chrome-devtools #{version}", shell_output("#{bin}/chrome-devtools --version")
  end
end
