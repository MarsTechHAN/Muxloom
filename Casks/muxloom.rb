cask "muxloom" do
  arch arm: "aarch64", intel: "x86_64"

  version :latest
  sha256 :no_check

  url "https://github.com/MarsTechHAN/Muxloom/releases/latest/download/muxloom-macos-#{arch}.tar.gz",
      verified: "github.com/MarsTechHAN/Muxloom/"
  name "Muxloom"
  desc "Terminal workspace for persistent AI coding sessions on local and SSH machines"
  homepage "https://github.com/MarsTechHAN/Muxloom"

  auto_updates true
  depends_on macos: :big_sur

  command_wrapper "muxloom",
                  executable: "#{staged_path}/muxloom/muxloom"
end
