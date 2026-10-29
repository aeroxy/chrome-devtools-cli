class ChromeDevtools < Formula
  desc "Chrome DevTools Protocol CLI — auto-connects to existing Chrome"
  homepage "https://github.com/aeroxy/chrome-devtools-cli"
  version "2.2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/chrome-devtools-cli/releases/download/2.2.0/chrome-devtools-macos-arm64.zip"
      sha256 "d71248cc2e8bb1af9ff68f550ad72bfd00b145fe16cf1347b97215535629186b"
    end
  end

  def install
    bin.install "chrome-devtools"
  end

  test do
    assert_match "chrome-devtools #{version}", shell_output("#{bin}/chrome-devtools --version")
  end
end
