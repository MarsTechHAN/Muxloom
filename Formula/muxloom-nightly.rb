# The nightly line, built here rather than downloaded. What runs on this
# machine — the controller and its own `muxloomd` — is compiled against this
# machine's toolchain; what does not run here cannot be: Homebrew's Rust has no
# standard library for other targets, so the companions for *other* machines
# are fetched from the nightly release into muxloom's cache the first time a
# machine needs one, and FFmpeg comes down as the same pinned static build the
# release bundle ships.
#
# Head-only on purpose. "Nightly" means the newest green commit on `main`, and
# a formula with no stable url never has to be version-bumped to keep saying
# that. The tagged releases stay with the `muxloom` cask, which downloads
# verified prebuilt binaries and can update itself in place.
class MuxloomNightly < Formula
  desc "Terminal workspace for persistent AI coding sessions, built from main"
  homepage "https://github.com/MarsTechHAN/Muxloom"
  license "GPL-3.0-only"
  head "https://github.com/MarsTechHAN/Muxloom.git", branch: "main"

  depends_on "rust" => :build

  # The controller decodes video previews with its own FFmpeg, found beside its
  # executable, rather than with whatever happens to be on PATH. Pinned to the
  # build CI ships so an install from source and one from the release archive
  # preview the same file the same way.
  on_macos do
    on_arm do
      resource "ffmpeg" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-arm64.gz"
        sha256 "8923876afa8db5585022d7860ec7e589af192f441c56793971276d450ed3bbfa"
      end

      resource "ffmpeg-license" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/darwin-arm64.LICENSE"
        sha256 "cb48bf09a11f5fb576cddb0431c8f5ed0a60157a9ec942adffc13907cbe083f2"
      end
    end

    on_intel do
      resource "ffmpeg" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-x64.gz"
        sha256 "929b375c1182d956c51f7ac25e0b2b0411fb01f6f407aa15c9758efeb4242106"
      end

      resource "ffmpeg-license" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/darwin-x64.LICENSE"
        sha256 "2e1d16c72fd74e12063776371da757322f8b77589386532f4fd8634bde7de1af"
      end
    end
  end

  on_linux do
    on_arm do
      resource "ffmpeg" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-linux-arm64.gz"
        sha256 "754a678672298bc68156adff58aa7385a592c2b30b1d0ae8750c45c915c4bac0"
      end

      resource "ffmpeg-license" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/linux-arm64.LICENSE"
        sha256 "8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903"
      end
    end

    on_intel do
      resource "ffmpeg" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-linux-x64.gz"
        sha256 "bfe8a8fc511530457b528c48d77b5737527b504a3797a9bc4866aeca69c2dffa"
      end

      resource "ffmpeg-license" do
        url "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/linux-x64.LICENSE"
        sha256 "8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903"
      end
    end
  end

  def install
    # Every nightly between two releases carries the same package version, so
    # the commit count is the only thing that orders them — and it needs the
    # whole history, which a shallow clone does not have. A count taken from a
    # shallow one would be a handful of commits and make every published
    # nightly look newer than this build forever; no stamp is better than that.
    quiet_system "git", "fetch", "--unshallow" if File.exist?(".git/shallow")
    unless File.exist?(".git/shallow")
      ENV["MUXLOOM_BUILD_HEIGHT"] = Utils.safe_popen_read("git", "rev-list", "--count", "HEAD").strip
    end
    ENV["MUXLOOM_BUILD_ID"] = Utils.safe_popen_read("git", "rev-parse", "HEAD").strip
    # A build knows which stream it came from, which is how this install keeps
    # being told about nightlies instead of being dragged back to releases.
    ENV["MUXLOOM_BUILD_CHANNEL"] = "nightly"

    # The companion is what gets uploaded to a machine of this architecture, so
    # it is built the way CI builds it: without the controller-only features it
    # would otherwise carry across the wire. The two feature sets go to roots of
    # their own because installing one package twice into one root makes cargo
    # drop the binaries the earlier install left behind.
    system "cargo", "install", "--no-default-features", "--bin", "muxloomd",
           *std_cargo_args(root: buildpath/"companion")
    libexec.install buildpath/"companion/bin/muxloomd"
    system "cargo", "install", "--bin", "muxloom",
           *std_cargo_args(root: buildpath/"controller")
    libexec.install buildpath/"controller/bin/muxloom"

    # muxloom finds its daemon, its companions, and its decoder relative to its
    # own executable, so the whole bundle lives in libexec and only the
    # controller is put on PATH.
    resource("ffmpeg").stage do
      libexec.install Dir["ffmpeg*"].first => "ffmpeg"
    end
    chmod 0755, libexec/"ffmpeg"
    resource("ffmpeg-license").stage do
      libexec.install Dir["*"].first => "ffmpeg.LICENSE"
    end

    # These files belong to Homebrew. muxloom reads this marker and reports a
    # new nightly instead of writing over a Cellar that would hand this build
    # straight back on the next upgrade.
    (libexec/".muxloom-managed").write "homebrew\n"

    # A wrapper rather than a symlink: macOS reports the path a process was
    # launched with, symlink and all, so a linked `muxloom` would look for its
    # daemon, its companions, and its decoder next to the link instead of next
    # to itself and find none of them.
    bin.write_exec_script libexec/"muxloom"
  end

  def caveats
    <<~EOS
      This is the nightly line: it was built from the newest commit on main.
      Pick up a later one with either of:

        muxloom update --nightly
        brew reinstall muxloom-nightly

      These files are Homebrew's, so `muxloom update` does not write over them
      — it runs the second command for you. `brew upgrade --fetch-HEAD
      muxloom-nightly` does the same job, but it asks GitHub's API whether main
      has moved and reports the install as current whenever it cannot ask, so
      it is the one to distrust when nothing seems to arrive.

      Companions for other architectures are not built locally; muxloom
      downloads them into its cache when a machine first needs one.

      The tagged releases are a separate package: `brew install --cask muxloom`.
      The two both provide `muxloom`, so uninstall one before linking the other.
    EOS
  end

  test do
    assert_match "muxloom", shell_output("#{bin}/muxloom --version")
    # What is on PATH has to reach the bundle, not stand beside an empty bin.
    assert_match libexec.to_s, (bin/"muxloom").read
    assert_path_exists libexec/"muxloomd"
    assert_match "ffmpeg version", shell_output("#{libexec}/ffmpeg -version")
    # The marker is what keeps a self-update off Homebrew's files.
    assert_equal "homebrew\n", (libexec/".muxloom-managed").read
  end
end
