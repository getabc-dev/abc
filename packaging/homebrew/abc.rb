# abc's Homebrew formula. It is rendered from the template packaging/homebrew/abc.rb in
# getabc-dev/abc by `cargo xtask release formula` for each release, which fills in the version
# and each archive's URL and sha256, and the release workflow pushes the result to
# getabc-dev/homebrew-tap as Formula/abc.rb. Never edit Formula/abc.rb in the tap by hand: the
# next release replaces it. Change the template.
class Abc < Formula
  desc "Self-hosted software factory for developers"
  homepage "https://github.com/getabc-dev/abc"
  version "@@version@@"
  license all_of: ["MIT", "BSD-3-Clause"]

  # abc keeps its data in a private Postgres 18 of its own, which `abc server setup` runs.
  depends_on "postgresql@18"

  on_macos do
    on_arm do
      url "@@url abc aarch64-apple-darwin@@"
      sha256 "@@sha256 abc aarch64-apple-darwin@@"

      resource "tailscaled" do
        url "@@url tailscaled aarch64-apple-darwin@@"
        sha256 "@@sha256 tailscaled aarch64-apple-darwin@@"
      end
    end
    on_intel do
      url "@@url abc x86_64-apple-darwin@@"
      sha256 "@@sha256 abc x86_64-apple-darwin@@"

      resource "tailscaled" do
        url "@@url tailscaled x86_64-apple-darwin@@"
        sha256 "@@sha256 tailscaled x86_64-apple-darwin@@"
      end
    end
  end

  on_linux do
    on_arm do
      url "@@url abc aarch64-unknown-linux-gnu@@"
      sha256 "@@sha256 abc aarch64-unknown-linux-gnu@@"

      resource "tailscaled" do
        url "@@url tailscaled aarch64-unknown-linux-gnu@@"
        sha256 "@@sha256 tailscaled aarch64-unknown-linux-gnu@@"
      end
    end
    on_intel do
      url "@@url abc x86_64-unknown-linux-gnu@@"
      sha256 "@@sha256 abc x86_64-unknown-linux-gnu@@"

      resource "tailscaled" do
        url "@@url tailscaled x86_64-unknown-linux-gnu@@"
        sha256 "@@sha256 tailscaled x86_64-unknown-linux-gnu@@"
      end
    end
  end

  def install
    bin.install "abc"
    # abc's own tailscaled, which abc runs as a separate tailnet node. It stays in
    # libexec, never linked onto the PATH, so it cannot shadow the user's own tailscale command.
    resource("tailscaled").stage { libexec.install "tailscaled" }
    # postgresql@18 is keg-only, off the PATH. abc finds its programs through this link in its
    # own libexec, to the formula's opt directory, which stays put across Postgres 18 updates.
    libexec.install_symlink formula_opt_bin("postgresql@18") => "postgresql"
  end

  test do
    assert_match "abc #{version}", shell_output("#{bin}/abc --version")
    assert_predicate libexec/"tailscaled", :executable?
    assert_match "@@tailscale_version@@", shell_output("#{libexec}/tailscaled --version")
    assert_predicate libexec/"postgresql/postgres", :executable?
    assert_predicate libexec/"postgresql/initdb", :executable?
    refute_path_exists bin/"tailscaled"
    refute_path_exists bin/"tailscale"
  end
end
