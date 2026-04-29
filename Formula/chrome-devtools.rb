class ChromeDevtools < Formula
  desc "Chrome DevTools Protocol CLI — auto-connects to existing Chrome"
  homepage "https://github.com/aeroxy/chrome-devtools-cli"
  version "0.1.2"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/chrome-devtools-cli/releases/download/0.1.2/chrome-devtools-macos-arm64.zip"
      sha256 "24010da8997666942342e64a9b753e787fb599ee74c797ea6d7fedaad07148fd"
    end
  end

  def install
    bin.install "chrome-devtools"
  end

  test do
    assert_match "chrome-devtools #{version}", shell_output("#{bin}/chrome-devtools --version")
  end
end
