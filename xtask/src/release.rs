//! Releases (brief decision 52): what the release workflow (`.github/workflows/release.yml`) runs
//! to build, package and describe abc for Homebrew, on a tag and as every pull request's dry run.
//!
//! - `tailscale-source` fetches Tailscale's source at the pin in [`TAILSCALE_PIN`], after checking
//!   that the pinned tag still names the pinned commit.
//! - `go` installs the Go that builds tailscaled into [`GO`]: the version Tailscale's `go.mod`
//!   names, from go.dev, once its archive matches the sha256 the pin records from go.dev.
//! - `package --target <triple>` builds the web app, then abc for the target with it embedded,
//!   and tailscaled from that source, and packages each with its licence as
//!   `abc-<version>-<target>.tar.gz` and `tailscaled-<version>-<target>.tar.gz` in [`DIST`]. A
//!   Linux binary must need no glibc newer than [`GLIBC_FLOOR`], and tailscaled none at all.
//! - `check-tag <tag>` fails unless the tag is `v` and the workspace's version, the one
//!   `abc --version` prints, and the tagged commit is on `origin`'s `main`.
//! - `formula` renders the Homebrew formula from [`FORMULA_TEMPLATE`] for the archives of all
//!   four targets, with their sha256s and URLs under a base URL: the release's download URL, or a
//!   `file://` directory for the dry run's install.
//! - `tap-plan` compares a rendered formula with the tap's current one and says how the tap job
//!   updates it, refusing to move the tap to an older version.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail, ensure};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Deserialize;

use crate::{git, tools, web};

/// The Tailscale version and commit the formula's tailscaled is built from, relative to the root.
pub(crate) const TAILSCALE_PIN: &str = "packaging/tailscale.toml";

/// The Homebrew formula's template, relative to the root.
pub(crate) const FORMULA_TEMPLATE: &str = "packaging/homebrew/abc.rb";

/// Where `package` writes the archives, relative to the root.
pub(crate) const DIST: &str = "target/dist";

/// Where `web` writes the built web app's archive, relative to the root.
pub(crate) const WEB: &str = "target/web";

/// Where `tailscale-source` puts Tailscale's source, relative to the root.
pub(crate) const TAILSCALE_SOURCE: &str = "target/tailscale";

/// Tailscale's repository.
pub(crate) const TAILSCALE_REPOSITORY: &str = "https://github.com/tailscale/tailscale.git";

/// Where `go` installs the Go that builds tailscaled, relative to the root.
pub(crate) const GO: &str = "target/go";

/// Where go.dev serves Go's release archives.
pub(crate) const GO_DOWNLOADS: &str = "https://go.dev/dl";

/// The platforms, in go.dev's names, that the release workflow runs Go on: macOS on Apple
/// silicon, which builds both macOS targets, and Linux on each architecture.
const GO_PLATFORMS: [&str; 3] = ["darwin-arm64", "linux-amd64", "linux-arm64"];

/// The newest glibc a Linux build of abc may need: that of `AlmaLinux` 8, which the release workflow
/// builds in, so abc runs on RHEL 8, Debian 10, Ubuntu 20.04 and anything newer.
pub(crate) const GLIBC_FLOOR: [u32; 2] = [2, 28];

/// The oldest macOS the builds run on: Go's own floor for tailscaled.
pub(crate) const MACOS_FLOOR: &str = "12.0";

/// A platform abc is released for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    AppleArm,
    AppleIntel,
    LinuxArm,
    LinuxIntel,
}

impl Target {
    /// Every target, in the order the formula lists them.
    pub(crate) const ALL: [Self; 4] = [
        Self::AppleArm,
        Self::AppleIntel,
        Self::LinuxArm,
        Self::LinuxIntel,
    ];

    /// The Rust target triple.
    pub(crate) const fn triple(self) -> &'static str {
        match self {
            Self::AppleArm => "aarch64-apple-darwin",
            Self::AppleIntel => "x86_64-apple-darwin",
            Self::LinuxArm => "aarch64-unknown-linux-gnu",
            Self::LinuxIntel => "x86_64-unknown-linux-gnu",
        }
    }

    /// Whether the target is Linux, else macOS.
    const fn linux(self) -> bool {
        match self {
            Self::LinuxArm | Self::LinuxIntel => true,
            Self::AppleArm | Self::AppleIntel => false,
        }
    }

    /// Go's name for the target's architecture.
    const fn go_arch(self) -> &'static str {
        match self {
            Self::AppleArm | Self::LinuxArm => "arm64",
            Self::AppleIntel | Self::LinuxIntel => "amd64",
        }
    }

    /// Parses a target triple, for clap.
    pub(crate) fn parse(triple: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|target| target.triple() == triple)
            .ok_or_else(|| {
                let known: Vec<&str> = Self::ALL.iter().map(|target| target.triple()).collect();
                format!("abc is released for {}, not {triple}", known.join(", "))
            })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.triple())
    }
}

/// A program a release archive carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Program {
    Abc,
    Tailscaled,
}

impl Program {
    const ALL: [Self; 2] = [Self::Abc, Self::Tailscaled];

    const fn name(self) -> &'static str {
        match self {
            Self::Abc => "abc",
            Self::Tailscaled => "tailscaled",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|program| program.name() == name)
    }
}

/// The versions one release carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Versions {
    /// abc's: the workspace's version.
    abc: String,
    /// The Tailscale release tailscaled is built from.
    tailscale: Pin,
}

impl Versions {
    /// The versions in the tree at `root`.
    pub(crate) fn read(root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            abc: workspace_version(root)?,
            tailscale: Pin::read(root)?,
        })
    }

    /// The archive of `program` for `target`.
    fn archive(&self, program: Program, target: Target) -> String {
        let version = match program {
            Program::Abc => &self.abc,
            Program::Tailscaled => &self.tailscale.version,
        };
        format!("{}-{version}-{target}.tar.gz", program.name())
    }
}

/// The workspace's version, from `[workspace.package]` in the root's `Cargo.toml`.
fn workspace_version(root: &Path) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct Manifest {
        workspace: Workspace,
    }
    #[derive(Deserialize)]
    struct Workspace {
        package: Package,
    }
    #[derive(Deserialize)]
    struct Package {
        version: String,
    }
    let manifest =
        fs::read_to_string(root.join("Cargo.toml")).context("could not read Cargo.toml")?;
    let manifest: Manifest = toml_edit::de::from_str(&manifest)
        .context("Cargo.toml has no [workspace.package] version")?;
    Ok(manifest.workspace.package.version)
}

/// The Tailscale release the formula's tailscaled is built from, and the Go that builds it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pin {
    version: String,
    commit: String,
    go: GoPin,
}

/// A Go release: its version and, for each of [`GO_PLATFORMS`], the sha256 go.dev publishes for
/// its archive.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoPin {
    version: String,
    sha256: BTreeMap<String, String>,
}

/// Whether `version` is a release number such as `1.2.3`.
fn is_release_number(version: &str) -> bool {
    !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Whether `text` is a sha in lowercase hex of `length` digits.
fn is_lowercase_hex(text: &str, length: usize) -> bool {
    text.len() == length && text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

impl Pin {
    /// The pin in [`TAILSCALE_PIN`].
    fn read(root: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(root.join(TAILSCALE_PIN))
            .with_context(|| format!("could not read {TAILSCALE_PIN}"))?;
        Self::parse(&text).with_context(|| format!("{TAILSCALE_PIN} is not a valid pin"))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let pin: Self = toml_edit::de::from_str(text)
            .context("it needs a version, a commit and a [go] table")?;
        ensure!(
            is_release_number(&pin.version),
            "the version must be a release number such as 1.2.3, not {:?}",
            pin.version
        );
        ensure!(
            is_lowercase_hex(&pin.commit, 40),
            "the commit must be a full lowercase sha, not {:?}",
            pin.commit
        );
        ensure!(
            is_release_number(&pin.go.version),
            "Go's version must be a release number such as 1.27.3, not {:?}",
            pin.go.version
        );
        let platforms: Vec<&str> = pin.go.sha256.keys().map(String::as_str).collect();
        ensure!(
            platforms == GO_PLATFORMS,
            "[go] sha256 must name exactly {}, not {}",
            GO_PLATFORMS.join(", "),
            platforms.join(", ")
        );
        for (platform, sha256) in &pin.go.sha256 {
            ensure!(
                is_lowercase_hex(sha256, 64),
                "Go's sha256 for {platform} must be 64 lowercase hex digits, not {sha256:?}"
            );
        }
        Ok(pin)
    }

    /// The tag Tailscale released this version under.
    fn tag(&self) -> String {
        format!("v{}", self.version)
    }

    /// The linker flags that stamp tailscaled with this version, as Tailscale's `build_dist.sh`
    /// does, stripping the symbol table as a distributed binary.
    fn ldflags(&self) -> String {
        let short = self.commit.get(..9).unwrap_or(&self.commit);
        format!(
            "-s -w -X tailscale.com/version.longStamp={version}-t{short} \
             -X tailscale.com/version.shortStamp={version} \
             -X tailscale.com/version.gitCommitStamp={commit}",
            version = self.version,
            commit = self.commit,
        )
    }
}

/// Fails unless `tag` releases the workspace's version at `root`, a release number, and `root`'s
/// HEAD, the tagged commit, is on `main` at `origin`, which it fetches: a release ships only
/// what `main`'s review and checks passed.
pub(crate) fn check_tag(root: &Path, tag: &str) -> anyhow::Result<()> {
    let version = workspace_version(root)?;
    ensure!(
        is_release_number(&version),
        "the workspace's version {version} is not a release number such as 1.2.3"
    );
    ensure!(
        tag == format!("v{version}"),
        "the tag {tag} does not release the workspace's version {version}: bump \
         [workspace.package] version in Cargo.toml and tag that commit v{version}"
    );
    let mut fetch = vec!["fetch", "--quiet", "--no-tags"];
    if git::git(root, &["rev-parse", "--is-shallow-repository"])?.trim() == "true" {
        fetch.push("--unshallow");
    }
    fetch.extend(["origin", "+refs/heads/main:refs/remotes/origin/main"]);
    git::git(root, &fetch)?;
    let head = git::git(root, &["rev-parse", "HEAD"])?;
    let on_main = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            "HEAD",
            "refs/remotes/origin/main",
        ])
        .current_dir(root)
        .status()
        .context("could not run git")?;
    ensure!(
        on_main.success(),
        "the tag {tag} names {}, which is not on origin's main: tag a commit of main",
        head.trim()
    );
    Ok(())
}

/// Fetches Tailscale's source at the pinned commit into [`TAILSCALE_SOURCE`], from `repository`,
/// once its pinned tag names that commit there; a checkout already at the commit is kept.
pub(crate) fn tailscale_source(
    root: &Path,
    repository: &str,
    out: &mut impl Write,
) -> anyhow::Result<PathBuf> {
    let pin = Pin::read(root)?;
    let tag = pin.tag();
    let listed = git::git(
        root,
        &[
            "ls-remote",
            repository,
            &format!("refs/tags/{tag}"),
            &format!("refs/tags/{tag}^{{}}"),
        ],
    )?;
    let tagged =
        tagged_commit(&listed, &tag).with_context(|| format!("{repository} has no tag {tag}"))?;
    ensure!(
        tagged == pin.commit,
        "{repository}'s tag {tag} names {tagged}, not the pinned {}: check the release and \
         update {TAILSCALE_PIN}",
        pin.commit
    );
    let source = root.join(TAILSCALE_SOURCE);
    if source.join(".git").is_dir()
        && git::git(&source, &["rev-parse", "HEAD"]).is_ok_and(|head| head.trim() == pin.commit)
        && git::git(&source, &["status", "--porcelain"]).is_ok_and(|status| status.is_empty())
    {
        writeln!(
            out,
            "xtask release: {} holds Tailscale {tag}",
            source.display()
        )?;
        return Ok(source);
    }
    if source.exists() {
        fs::remove_dir_all(&source)
            .with_context(|| format!("could not clear {}", source.display()))?;
    }
    fs::create_dir_all(&source)
        .with_context(|| format!("could not create {}", source.display()))?;
    git::git(&source, &["init", "--quiet"])?;
    git::git(
        &source,
        &["fetch", "--quiet", "--depth", "1", repository, &pin.commit],
    )?;
    git::git(&source, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
    let head = git::git(&source, &["rev-parse", "HEAD"])?;
    ensure!(
        head.trim() == pin.commit,
        "the fetch checked out {}, not {}",
        head.trim(),
        pin.commit
    );
    writeln!(
        out,
        "xtask release: fetched Tailscale {tag} into {}",
        source.display()
    )?;
    Ok(source)
}

/// The Go version a `go.mod` names: its `toolchain` directive's when it has one, else its `go`
/// directive's.
fn go_mod_version(go_mod: &str) -> Option<String> {
    let directive = |name: &str| {
        go_mod.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(name))
                .then(|| words.next())
                .flatten()
                .map(str::to_owned)
        })
    };
    directive("toolchain")
        .and_then(|toolchain| toolchain.strip_prefix("go").map(str::to_owned))
        .or_else(|| directive("go"))
}

/// Fails unless Tailscale's source at `root` names the Go version `pin` pins.
fn check_go_mod(root: &Path, pin: &Pin) -> anyhow::Result<()> {
    let path = root.join(TAILSCALE_SOURCE).join("go.mod");
    let go_mod = fs::read_to_string(&path).with_context(|| {
        format!(
            "could not read {}: run `cargo xtask release tailscale-source` first",
            path.display()
        )
    })?;
    let named = go_mod_version(&go_mod)
        .with_context(|| format!("{} names no Go version", path.display()))?;
    ensure!(
        named == pin.go.version,
        "Tailscale {} builds with Go {named}, but {TAILSCALE_PIN} pins Go {}: pin Go {named} with \
         the sha256s go.dev publishes for it",
        pin.version,
        pin.go.version
    );
    Ok(())
}

/// go.dev's name for the platform of `os` and `arch` (Rust's names), when the release workflow
/// runs Go there.
fn go_platform(os: &str, arch: &str) -> anyhow::Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        _ => bail!(
            "the release builds on {}, not {os} on {arch}",
            GO_PLATFORMS.join(", ")
        ),
    }
}

/// Installs the pinned Go for the machine of `os` and `arch` (Rust's names) into [`GO`],
/// downloading its archive from `downloads` with `fetch`, once Tailscale's `go.mod` names that
/// version and the archive matches the pinned sha256.
pub(crate) fn go(
    root: &Path,
    (os, arch): (&str, &str),
    downloads: &str,
    fetch: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let pin = Pin::read(root)?;
    check_go_mod(root, &pin)?;
    let platform = go_platform(os, arch)?;
    let version = &pin.go.version;
    let expected = pin
        .go
        .sha256
        .get(platform)
        .with_context(|| format!("{TAILSCALE_PIN} pins no Go for {platform}"))?;
    let url = format!("{downloads}/go{version}.{platform}.tar.gz");
    let archive = fetch(&url)?;
    let found = tools::sha256_hex(&archive);
    ensure!(
        &found == expected,
        "{url} has sha256 {found}, not the {expected} {TAILSCALE_PIN} pins from go.dev"
    );
    let go = root.join(GO);
    let unpacked = root.join(format!("{GO}.unpacked"));
    for dir in [&go, &unpacked] {
        if dir.exists() {
            fs::remove_dir_all(dir)
                .with_context(|| format!("could not clear {}", dir.display()))?;
        }
    }
    tar::Archive::new(flate2::read::GzDecoder::new(archive.as_slice()))
        .unpack(&unpacked)
        .with_context(|| format!("could not unpack {url}"))?;
    fs::rename(unpacked.join("go"), &go).with_context(|| format!("{url} holds no go directory"))?;
    fs::remove_dir_all(&unpacked)
        .with_context(|| format!("could not clear {}", unpacked.display()))?;
    check_go(root, &pin)?;
    writeln!(
        out,
        "xtask release: installed Go {version} for {platform} into {}",
        go.display()
    )?;
    Ok(())
}

/// Fails unless [`GO`] holds the Go `pin` pins, by the `VERSION` file at its root.
fn check_go(root: &Path, pin: &Pin) -> anyhow::Result<()> {
    let path = root.join(GO).join("VERSION");
    let installed = fs::read_to_string(&path).unwrap_or_default();
    let expected = format!("go{}", pin.go.version);
    ensure!(
        installed.lines().next() == Some(expected.as_str()),
        "{} does not hold {expected}: run `cargo xtask release go` first",
        root.join(GO).display()
    );
    Ok(())
}

/// The commit `ls-remote`'s `listed` refs give `tag`: the peeled commit of an annotated tag, else
/// the tag's own.
fn tagged_commit(listed: &str, tag: &str) -> Option<String> {
    let refs: Vec<(&str, &str)> = listed
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    let find = |name: String| {
        refs.iter()
            .find(|&&(_, reference)| reference == name)
            .map(|&(sha, _)| sha.to_owned())
    };
    find(format!("refs/tags/{tag}^{{}}")).or_else(|| find(format!("refs/tags/{tag}")))
}

/// Runs a command and says whether it passed.
pub(crate) type Runner<'run> = dyn FnMut(&mut Command) -> anyhow::Result<bool> + 'run;

/// The archive [`pack_web`] packs the web app built at `commit` into, relative to the root.
fn web_archive(commit: &str) -> String {
    format!("{WEB}/web-{commit}.tar.gz")
}

/// The commit `root`'s HEAD names.
fn head(root: &Path) -> anyhow::Result<String> {
    Ok(git::git(root, &["rev-parse", "HEAD"])?.trim().to_owned())
}

/// Builds the web app at `root` with the pinned node and pnpm, running each step with `run`, and
/// packs `web/dist` into [`web_archive`] for HEAD: the one web app every target's `package`
/// embeds, built once on a machine node runs on. Reports whether it passed, and says in `out`
/// what failed and how to fix it.
pub(crate) fn pack_web(
    root: &Path,
    toolchain: &web::Toolchain,
    run: &mut Runner<'_>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !web::present(root) {
        writeln!(out, "xtask release: {}", web::MISSING)?;
        return Ok(false);
    }
    if let Some(step) = web::build(root, toolchain, run)? {
        writeln!(out, "xtask release: {} failed: {}", step.name, step.fix)?;
        return Ok(false);
    }
    let dist = root.join("web/dist");
    ensure!(
        web::built(root),
        "the web build left no {}",
        root.join(web::BUILT).display()
    );
    let mut files = Vec::new();
    web_files(&dist, &dist, &mut files)?;
    files.sort();
    let path = root.join(web_archive(&head(root)?));
    let dir = root.join(WEB);
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let entries: Vec<(&str, PathBuf, u32)> = files
        .iter()
        .map(|(name, file)| (name.as_str(), file.clone(), 0o644))
        .collect();
    archive(&path, "dist", &entries, commit_time(root)?)?;
    writeln!(
        out,
        "xtask release: packed the web app into {}",
        path.display()
    )?;
    Ok(true)
}

/// Adds every file under `dir` to `files`, as its path relative to `dist` and its full path.
fn web_files(dist: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> anyhow::Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("could not list {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("could not list {}", dir.display()))?
            .path();
        let kind = fs::symlink_metadata(&path)
            .with_context(|| format!("could not inspect {}", path.display()))?
            .file_type();
        if kind.is_dir() {
            web_files(dist, &path, files)?;
        } else {
            ensure!(
                kind.is_file(),
                "{} is neither a file nor a directory",
                path.display()
            );
            let relative = path
                .strip_prefix(dist)
                .context("a file outside web/dist")?
                .to_str()
                .with_context(|| format!("{} is not UTF-8", path.display()))?
                .to_owned();
            files.push((relative, path));
        }
    }
    Ok(())
}

/// Puts the web app [`pack_web`] packed for HEAD in place as `web/dist`, replacing whatever was
/// there, so the binary embeds exactly that build. Says in `out` how to make the archive when
/// it is missing, and reports whether it was there.
fn unpack_web(root: &Path, out: &mut impl Write) -> anyhow::Result<bool> {
    let packed = root.join(web_archive(&head(root)?));
    if !packed.is_file() {
        writeln!(
            out,
            "xtask release: {} holds no web app built at HEAD: run `cargo xtask release web` \
             first, on this commit (the release workflow's web job does, and every build \
             downloads its archive)",
            packed.display()
        )?;
        return Ok(false);
    }
    let unpacked = root.join(WEB).join("unpacked");
    let dist = root.join("web/dist");
    for dir in [&unpacked, &dist] {
        if dir.exists() {
            fs::remove_dir_all(dir)
                .with_context(|| format!("could not clear {}", dir.display()))?;
        }
    }
    let file =
        fs::File::open(&packed).with_context(|| format!("could not read {}", packed.display()))?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(&unpacked)
        .with_context(|| format!("could not unpack {}", packed.display()))?;
    fs::rename(unpacked.join("dist"), &dist)
        .with_context(|| format!("{} holds no dist directory", packed.display()))?;
    fs::remove_dir_all(&unpacked)
        .with_context(|| format!("could not clear {}", unpacked.display()))?;
    Ok(true)
}

/// Builds abc, with the web app [`pack_web`] packed for HEAD, and tailscaled for `target`, and
/// packages them in [`DIST`], running each command with `run`; reports whether every step
/// passed, and says in `out` what failed and how to fix it. It runs no node: the web app is
/// built once, on a machine node runs on, rather than once per target in its build environment.
pub(crate) fn package(
    root: &Path,
    target: Target,
    run: &mut Runner<'_>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let versions = Versions::read(root)?;
    let source = root.join(TAILSCALE_SOURCE);
    if !source.join("go.mod").is_file() {
        writeln!(
            out,
            "xtask release: {} holds no Tailscale source: run `cargo xtask release \
             tailscale-source` first",
            source.display()
        )?;
        return Ok(false);
    }
    if let Err(error) =
        check_go_mod(root, &versions.tailscale).and_then(|()| check_go(root, &versions.tailscale))
    {
        writeln!(out, "xtask release: {error}")?;
        return Ok(false);
    }
    if !unpack_web(root, out)? {
        return Ok(false);
    }
    let target_dir = web::target_dir(root);
    let built = target_dir.join(target.triple());
    for (command, failure) in [
        (
            rustup_target(root, target),
            "rustup could not add the target",
        ),
        (
            cargo_build(root, &target_dir, target),
            "cargo build failed: fix each error above",
        ),
        (
            go_build(
                &root.join(GO).join("bin/go"),
                &source,
                &built.join("tailscaled"),
                target,
                &versions.tailscale,
            ),
            "go build of tailscaled failed: fix each error above; the Go version is the one \
             Tailscale's go.mod names",
        ),
    ] {
        let mut command = command;
        if !run(&mut command)? {
            writeln!(out, "xtask release: {target}: {failure}")?;
            return Ok(false);
        }
    }
    let abc = built.join("release/abc");
    let tailscaled = built.join("tailscaled");
    if target.linux() {
        for (binary, floor) in [(&abc, Some(GLIBC_FLOOR)), (&tailscaled, None)] {
            let bytes =
                fs::read(binary).with_context(|| format!("could not read {}", binary.display()))?;
            if let Err(error) = within_glibc(&bytes, floor) {
                writeln!(out, "xtask release: {}: {error}", binary.display())?;
                return Ok(false);
            }
        }
    }
    let dist = root.join(DIST);
    fs::create_dir_all(&dist).with_context(|| format!("could not create {}", dist.display()))?;
    let mtime = commit_time(root)?;
    for (program, binary, licences) in [
        (Program::Abc, &abc, vec![("LICENSE", root.join("LICENSE"))]),
        (
            Program::Tailscaled,
            &tailscaled,
            vec![
                ("LICENSE", source.join("LICENSE")),
                ("PATENTS", source.join("PATENTS")),
            ],
        ),
    ] {
        let name = versions.archive(program, target);
        let mut files = vec![(program.name(), binary.clone(), 0o755)];
        files.extend(
            licences
                .into_iter()
                .map(|(licence, path)| (licence, path, 0o644)),
        );
        let path = dist.join(&name);
        archive(&path, name.trim_end_matches(".tar.gz"), &files, mtime)?;
        writeln!(out, "xtask release: packaged {}", path.display())?;
    }
    Ok(true)
}

/// `rustup target add` for `target`, in `root` so the pinned toolchain gets it.
fn rustup_target(root: &Path, target: Target) -> Command {
    let mut command = Command::new("rustup");
    command
        .args(["target", "add", target.triple()])
        .current_dir(root);
    command
}

/// The release build of abc for `target` into `target_dir`.
fn cargo_build(root: &Path, target_dir: &Path, target: Target) -> Command {
    let mut command = Command::new(crate::check::cargo());
    command
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "abc",
            "--target",
            target.triple(),
            "--target-dir",
        ])
        .arg(target_dir)
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(root);
    if !target.linux() {
        command.env("MACOSX_DEPLOYMENT_TARGET", MACOS_FLOOR);
    }
    command
}

/// The build of tailscaled from `source` for `target` into `output` with the Go at `go`. Linux
/// builds are static, as Tailscale's own are; macOS builds use cgo, as Homebrew's do, with the
/// platform's clang. Go never downloads another toolchain, and the modules are go.sum's.
fn go_build(go: &Path, source: &Path, output: &Path, target: Target, pin: &Pin) -> Command {
    let mut command = Command::new(go);
    command
        .args(["build", "-trimpath", "-ldflags", &pin.ldflags(), "-o"])
        .arg(output)
        .arg("./cmd/tailscaled")
        .current_dir(source)
        .env("GOOS", if target.linux() { "linux" } else { "darwin" })
        .env("GOARCH", target.go_arch())
        .env("GOTOOLCHAIN", "local")
        .env("GOFLAGS", "-mod=readonly");
    if target.linux() {
        command.env("CGO_ENABLED", "0");
    } else {
        let arch = match target.go_arch() {
            "amd64" => "x86_64",
            other => other,
        };
        let flags = format!("-arch {arch} -mmacosx-version-min={MACOS_FLOOR}");
        command
            .env("CGO_ENABLED", "1")
            .env("CC", "clang")
            .env("CGO_CFLAGS", &flags)
            .env("CGO_LDFLAGS", &flags);
    }
    command
}

/// The glibc versions the ELF `binary` needs: the `GLIBC_` entries of its GNU version-needed
/// table (`.gnu.version_r`), such as `[2, 28]` for `GLIBC_2.28`. A static binary has no such
/// table and needs none. A string that merely contains `GLIBC_` elsewhere in the binary is no
/// need. A `GLIBC_` entry that is no version number, such as `GLIBC_ABI_DT_RELR`, fails, since
/// no floor accounts for it.
fn glibc_versions(binary: &[u8]) -> anyhow::Result<BTreeSet<Vec<u32>>> {
    use object::read::elf::{FileHeader as _, SectionTable};

    let header = object::elf::FileHeader64::<object::Endianness>::parse(binary)
        .context("it is not a 64-bit ELF binary")?;
    let endian = header
        .endian()
        .context("its ELF header names no byte order")?;
    let sections: SectionTable<'_, _> = header
        .sections(endian, binary)
        .context("its ELF section table is unreadable")?;
    let mut found = BTreeSet::new();
    let Some((mut needs, link)) = sections
        .gnu_verneed(endian, binary)
        .context("its version-needed table is unreadable")?
    else {
        return Ok(found);
    };
    let strings = sections
        .strings(endian, binary, link)
        .context("its version-needed table names no string table")?;
    while let Some((_, mut entries)) = needs
        .next()
        .context("its version-needed table is unreadable")?
    {
        while let Some(entry) = entries
            .next()
            .context("its version-needed table is unreadable")?
        {
            let name = entry
                .name(endian, strings)
                .context("its version-needed table names a missing string")?;
            let name = String::from_utf8_lossy(name);
            let Some(version) = name.strip_prefix("GLIBC_") else {
                continue;
            };
            let parts: Option<Vec<u32>> =
                version.split('.').map(|part| part.parse().ok()).collect();
            found.insert(
                parts
                    .filter(|parts| parts.len() >= 2)
                    .with_context(|| format!("it needs {name}, which is no glibc version"))?,
            );
        }
    }
    Ok(found)
}

/// Fails when `binary` needs a glibc newer than `floor`, or any glibc when `floor` is `None`.
fn within_glibc(binary: &[u8], floor: Option<[u32; 2]>) -> anyhow::Result<()> {
    let versions = glibc_versions(binary)?;
    let Some(floor) = floor else {
        ensure!(
            versions.is_empty(),
            "a static build must need no glibc, but it needs {}",
            dotted(versions.iter())
        );
        return Ok(());
    };
    let newer: Vec<&Vec<u32>> = versions
        .iter()
        .filter(|version| version.as_slice() > floor.as_slice())
        .collect();
    ensure!(
        newer.is_empty(),
        "it needs glibc {}, newer than the floor {}: build it in the release workflow's \
         AlmaLinux 8 container",
        dotted(newer.into_iter()),
        dotted([&floor.to_vec()].into_iter())
    );
    Ok(())
}

/// Versions as `2.28, 2.34`.
fn dotted<'version>(versions: impl Iterator<Item = &'version Vec<u32>>) -> String {
    versions
        .map(|version| {
            version
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The commit time of `root`'s HEAD, in seconds: every archive of a commit carries it, so the
/// same commit packs the same archives.
fn commit_time(root: &Path) -> anyhow::Result<u64> {
    let time = git::git(root, &["log", "-1", "--format=%ct"])?;
    time.trim()
        .parse()
        .with_context(|| format!("git gave {time:?} as HEAD's commit time"))
}

/// Writes a gzipped tarball at `path` holding `files` (name in the archive, path, mode) under the
/// directory `top`, owned by root and dated `mtime`.
fn archive(
    path: &Path,
    top: &str,
    files: &[(&str, PathBuf, u32)],
    mtime: u64,
) -> anyhow::Result<()> {
    let file =
        fs::File::create(path).with_context(|| format!("could not create {}", path.display()))?;
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::best()));
    for (name, source, mode) in files {
        let bytes =
            fs::read(source).with_context(|| format!("could not read {}", source.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len())?);
        header.set_mode(*mode);
        header.set_mtime(mtime);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut header, format!("{top}/{name}"), bytes.as_slice())
            .with_context(|| format!("could not add {name} to {}", path.display()))?;
    }
    tar.into_inner()
        .context("could not finish the archive")?
        .finish()
        .context("could not finish the archive")?
        .sync_all()
        .with_context(|| format!("could not write {}", path.display()))
}

/// The sha256 of the file at `path`, in hex.
fn sha256(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(tools::sha256_hex(&bytes))
}

/// Renders the formula at `root` for the archives in `dist`, each at `base_url/<archive>`.
pub(crate) fn formula(root: &Path, dist: &Path, base_url: &str) -> anyhow::Result<String> {
    let template = fs::read_to_string(root.join(FORMULA_TEMPLATE))
        .with_context(|| format!("could not read {FORMULA_TEMPLATE}"))?;
    let versions = Versions::read(root)?;
    render(&template, &versions, base_url, &|archive| {
        sha256(&dist.join(archive)).with_context(|| {
            format!(
                "{archive} is missing from {}: download every target's archives first",
                dist.display()
            )
        })
    })
    .with_context(|| format!("could not render {FORMULA_TEMPLATE}"))
}

/// Fills in `template`'s `@@…@@` fields: `version`, `tailscale_version`, and `url` or `sha256`
/// with a program and a target, such as `@@url abc aarch64-apple-darwin@@`. `digest` gives an
/// archive's sha256. Every program must have both fields for every target.
fn render(
    template: &str,
    versions: &Versions,
    base_url: &str,
    digest: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let base_url = base_url.trim_end_matches('/');
    let mut rendered = String::new();
    let mut used = BTreeSet::new();
    for (index, part) in template.split("@@").enumerate() {
        if index.is_multiple_of(2) {
            rendered.push_str(part);
            continue;
        }
        let words: Vec<&str> = part.split(' ').collect();
        let value = match words.as_slice() {
            ["version"] => versions.abc.clone(),
            ["tailscale_version"] => versions.tailscale.version.clone(),
            [field @ ("url" | "sha256"), program, triple] => {
                let program = Program::parse(program)
                    .with_context(|| format!("no program {program} in @@{part}@@"))?;
                let target = Target::parse(triple).map_err(anyhow::Error::msg)?;
                used.insert((*field, program, target));
                let archive = versions.archive(program, target);
                if *field == "url" {
                    format!("{base_url}/{archive}")
                } else {
                    digest(&archive)?
                }
            }
            _ => bail!("unknown field @@{part}@@"),
        };
        rendered.push_str(&value);
    }
    ensure!(
        template.matches("@@").count().is_multiple_of(2),
        "an @@ field is not closed"
    );
    let missing: Vec<String> = ["url", "sha256"]
        .into_iter()
        .flat_map(|field| {
            Program::ALL.into_iter().flat_map(move |program| {
                Target::ALL
                    .into_iter()
                    .map(move |target| (field, program, target))
            })
        })
        .filter(|key| !used.contains(key))
        .map(|(field, program, target)| format!("@@{field} {} {target}@@", program.name()))
        .collect();
    ensure!(
        missing.is_empty(),
        "the template lacks {}",
        missing.join(", ")
    );
    Ok(rendered)
}

/// How the tap job brings `Formula/abc.rb` in the tap to a rendered formula.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TapPlan {
    /// The tap has no formula yet: create it.
    Create,
    /// Replace the tap's formula, the blob with this sha: GitHub refuses the push if the tap
    /// moved since.
    Update(String),
    /// The tap holds this very formula already.
    Keep,
}

impl fmt::Display for TapPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create => formatter.write_str("create"),
            Self::Update(sha) => write!(formatter, "update {sha}"),
            Self::Keep => formatter.write_str("keep"),
        }
    }
}

/// The version a formula declares, from its `version "…"` line, as numbers.
fn formula_version(formula: &str) -> anyhow::Result<Vec<u32>> {
    let version = formula
        .lines()
        .find_map(|line| line.trim().strip_prefix("version \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .context("the formula declares no version")?;
    version
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u32>>>()
        .with_context(|| format!("the formula's version {version:?} is not a release number"))
}

/// Plans the tap's update to `formula` from `current`, the tap's `Formula/abc.rb` as GitHub's
/// contents API gives it, or `None` when the tap has none. It refuses a formula older than the
/// tap's, so a release never moves the tap backwards.
pub(crate) fn tap_plan(formula: &str, current: Option<&str>) -> anyhow::Result<TapPlan> {
    use base64::Engine as _;

    #[derive(Deserialize)]
    struct Contents {
        sha: String,
        content: String,
        encoding: String,
    }
    let release = formula_version(formula).context("could not read the rendered formula")?;
    let Some(current) = current else {
        return Ok(TapPlan::Create);
    };
    let contents: Contents = serde_json::from_str(current)
        .context("the tap's formula is not a GitHub contents response")?;
    ensure!(
        contents.encoding == "base64",
        "GitHub gave the tap's formula in {}, not base64",
        contents.encoding
    );
    let text: String = contents.content.split_whitespace().collect();
    let tap = base64::engine::general_purpose::STANDARD
        .decode(text)
        .context("the tap's formula is not base64")?;
    let tap = String::from_utf8(tap).context("the tap's formula is not UTF-8")?;
    let tapped = formula_version(&tap).context("could not read the tap's formula")?;
    let dotted = |version: &[u32]| {
        version
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    };
    ensure!(
        tapped <= release,
        "the tap holds abc {}, newer than this release's {}: a release never moves the tap \
         backwards",
        dotted(&tapped),
        dotted(&release)
    );
    Ok(if tap == formula {
        TapPlan::Keep
    } else {
        TapPlan::Update(contents.sha)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fmt::Write as _;
    use std::fs;
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{
        FORMULA_TEMPLATE, GoPin, Pin, Program, TAILSCALE_PIN, TapPlan, Target, Versions, archive,
        check_tag, formula, glibc_versions, go, go_mod_version, go_platform, pack_web, package,
        render, tagged_commit, tailscale_source, tap_plan, within_glibc,
    };
    use crate::testing::{fake_web, git, repository};

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A pin of Tailscale 1.104.5 at `commit`, built with Go 1.27.3 whose archives have the
    /// sha256 `go_sha256` on every platform.
    fn pin_text(commit: &str, go_sha256: &str) -> String {
        format!(
            "version = \"1.104.5\"\ncommit = \"{commit}\"\n\n[go]\nversion = \"1.27.3\"\n\
             sha256.darwin-arm64 = \"{go_sha256}\"\nsha256.linux-amd64 = \"{go_sha256}\"\n\
             sha256.linux-arm64 = \"{go_sha256}\"\n"
        )
    }

    /// A minimal 64-bit little-endian ELF file whose version-needed table (`.gnu.version_r`)
    /// lists `needs`, each a library and the versions needed from it, and which carries `data`
    /// in a section of its own. With no needs it has no such table, as a static binary has none.
    fn elf(needs: &[(&str, &[&str])], data: &[u8]) -> Vec<u8> {
        fn u16_of(value: usize) -> [u8; 2] {
            u16::try_from(value).unwrap().to_le_bytes()
        }
        fn u32_of(value: usize) -> [u8; 4] {
            u32::try_from(value).unwrap().to_le_bytes()
        }
        fn u64_of(value: usize) -> [u8; 8] {
            u64::try_from(value).unwrap().to_le_bytes()
        }
        fn pad(bytes: &mut Vec<u8>) {
            while !bytes.len().is_multiple_of(8) {
                bytes.push(0);
            }
        }
        let mut strings = vec![0_u8];
        let mut verneed = Vec::new();
        for (index, (file, versions)) in needs.iter().enumerate() {
            let last_need = index.checked_add(1) == Some(needs.len());
            let size = versions
                .len()
                .checked_mul(16)
                .unwrap()
                .checked_add(16)
                .unwrap();
            verneed.extend(u16_of(1));
            verneed.extend(u16_of(versions.len()));
            verneed.extend(u32_of(strings.len()));
            strings.extend(file.bytes().chain([0]));
            verneed.extend(u32_of(16));
            verneed.extend(u32_of(if last_need { 0 } else { size }));
            for (at, version) in versions.iter().enumerate() {
                let last_version = at.checked_add(1) == Some(versions.len());
                verneed.extend([0; 6]);
                verneed.extend(u16_of(at.checked_add(2).unwrap()));
                verneed.extend(u32_of(strings.len()));
                strings.extend(version.bytes().chain([0]));
                verneed.extend(u32_of(if last_version { 0 } else { 16 }));
            }
        }
        let names = b"\0.dynstr\0.gnu.version_r\0.rodata\0.shstrtab\0";
        // Each section: name offset, type, link, info, contents.
        let mut sections: Vec<(usize, u32, usize, usize, &[u8])> =
            vec![(0, 0, 0, 0, &[]), (1, 3, 0, 0, &strings)];
        if !needs.is_empty() {
            sections.push((9, 0x6fff_fffe, 1, needs.len(), &verneed));
        }
        sections.push((24, 1, 0, 0, data));
        sections.push((32, 3, 0, 0, names));
        let mut file = vec![0_u8; 64];
        let mut offsets = Vec::new();
        for (_, _, _, _, contents) in &sections {
            offsets.push(file.len());
            file.extend_from_slice(contents);
            pad(&mut file);
        }
        let headers = file.len();
        for ((name, kind, link, info, contents), offset) in sections.iter().zip(offsets) {
            file.extend(u32_of(*name));
            file.extend(kind.to_le_bytes());
            file.extend([0; 16]);
            file.extend(u64_of(offset));
            file.extend(u64_of(contents.len()));
            file.extend(u32_of(*link));
            file.extend(u32_of(*info));
            file.extend(u64_of(1));
            file.extend(u64_of(0));
        }
        let mut header = Vec::new();
        header.extend(b"\x7fELF\x02\x01\x01");
        header.extend([0; 9]);
        header.extend(u16_of(3));
        header.extend(u16_of(62));
        header.extend(u32_of(1));
        header.extend([0; 16]);
        header.extend(u64_of(headers));
        header.extend([0; 4]);
        header.extend(u16_of(64));
        header.extend(u16_of(0));
        header.extend(u16_of(0));
        header.extend(u16_of(64));
        header.extend(u16_of(sections.len()));
        header.extend(u16_of(sections.len().checked_sub(1).unwrap()));
        file.splice(..64, header);
        file
    }

    /// An abc built for glibc: one that needs libc's `versions`.
    fn linked(versions: &[&str]) -> Vec<u8> {
        elf(&[("libc.so.6", versions)], b"abc")
    }

    /// A repository at `dir` holding version 1.2.3 of a workspace, its licence, a Tailscale pin to
    /// `commit` and the real formula template.
    fn tree(dir: &Path, commit: &str) -> PathBuf {
        let root = repository(dir);
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        fs::write(root.join("LICENSE"), "MIT licence\n").unwrap();
        fs::create_dir_all(root.join("packaging/homebrew")).unwrap();
        fs::write(root.join(TAILSCALE_PIN), pin_text(commit, &"0".repeat(64))).unwrap();
        fs::copy(
            crate::testing::checkout().join(FORMULA_TEMPLATE),
            root.join(FORMULA_TEMPLATE),
        )
        .unwrap();
        root
    }

    fn versions() -> Versions {
        Versions {
            abc: "1.2.3".to_owned(),
            tailscale: Pin::parse(&pin_text(COMMIT, &"0".repeat(64))).unwrap(),
        }
    }

    #[test]
    fn names_the_four_targets() {
        let triples: Vec<String> = Target::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(
            triples,
            [
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "aarch64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu"
            ],
            "the release's four targets"
        );
        for target in Target::ALL {
            assert_eq!(
                Target::parse(target.triple()),
                Ok(target),
                "a triple parses back"
            );
        }
        assert_eq!(
            Target::parse("x86_64-pc-windows-msvc"),
            Err(
                "abc is released for aarch64-apple-darwin, x86_64-apple-darwin, \
                 aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu, not x86_64-pc-windows-msvc"
                    .to_owned()
            ),
            "another triple is refused, naming the known ones"
        );
    }

    #[test]
    fn names_each_archive_by_program_version_and_target() {
        let versions = versions();
        assert_eq!(
            versions.archive(Program::Abc, Target::LinuxArm),
            "abc-1.2.3-aarch64-unknown-linux-gnu.tar.gz",
            "abc's archive carries abc's version"
        );
        assert_eq!(
            versions.archive(Program::Tailscaled, Target::AppleIntel),
            "tailscaled-1.104.5-x86_64-apple-darwin.tar.gz",
            "tailscaled's carries Tailscale's"
        );
    }

    #[test]
    fn reads_the_tailscale_pin() {
        let sha = "a".repeat(64);
        let pin = Pin::parse(&pin_text(COMMIT, &sha)).unwrap();
        assert_eq!(
            pin.go,
            GoPin {
                version: "1.27.3".to_owned(),
                sha256: ["darwin-arm64", "linux-amd64", "linux-arm64"]
                    .into_iter()
                    .map(|platform| (platform.to_owned(), sha.clone()))
                    .collect(),
            },
            "with the Go that builds it"
        );
        assert_eq!(pin.tag(), "v1.104.5", "Tailscale tags releases with a v");
        assert_eq!(
            pin.ldflags(),
            format!(
                "-s -w -X tailscale.com/version.longStamp=1.104.5-t012345678 \
                 -X tailscale.com/version.shortStamp=1.104.5 \
                 -X tailscale.com/version.gitCommitStamp={COMMIT}"
            ),
            "the stamps Tailscale's build_dist.sh sets"
        );
        let good = pin_text(COMMIT, &sha);
        for (text, error) in [
            (
                format!("extra = 1\n{good}"),
                "it needs a version, a commit and a [go] table",
            ),
            (
                good.replace("version = \"1.104.5\"\n", ""),
                "it needs a version, a commit and a [go] table",
            ),
            (
                format!("version = \"1.104.5\"\ncommit = \"{COMMIT}\"\n"),
                "it needs a version, a commit and a [go] table",
            ),
            (
                good.replace("\"1.104.5\"", "\"v1.104.5\""),
                "the version must be a release number such as 1.2.3, not \"v1.104.5\"",
            ),
            (
                good.replace("\"1.104.5\"", "\"\""),
                "the version must be a release number such as 1.2.3, not \"\"",
            ),
            (
                good.replace("\"1.104.5\"", "\"1..5\""),
                "the version must be a release number such as 1.2.3, not \"1..5\"",
            ),
            (
                good.replace(COMMIT, "0123abc"),
                "the commit must be a full lowercase sha, not \"0123abc\"",
            ),
            (
                good.replace(COMMIT, &COMMIT.to_uppercase()),
                "the commit must be a full lowercase sha",
            ),
            (
                good.replace(COMMIT, &format!("{}g", COMMIT.get(1..).unwrap())),
                "the commit must be a full lowercase sha",
            ),
            (
                good.replace("\"1.27.3\"", "\"go1.27.3\""),
                "Go's version must be a release number such as 1.27.3, not \"go1.27.3\"",
            ),
            (
                good.replace("linux-arm64", "windows-amd64"),
                "[go] sha256 must name exactly darwin-arm64, linux-amd64, linux-arm64, not \
                 darwin-arm64, linux-amd64, windows-amd64",
            ),
            (
                good.replacen(&sha, &"A".repeat(64), 1),
                "Go's sha256 for darwin-arm64 must be 64 lowercase hex digits",
            ),
            (
                good.replacen(&sha, &"a".repeat(63), 1),
                "Go's sha256 for darwin-arm64 must be 64 lowercase hex digits",
            ),
        ] {
            let found = format!("{:#}", Pin::parse(&text).unwrap_err());
            assert!(found.contains(error), "{text:?}: {found}");
        }
    }

    #[test]
    fn checks_the_tag_against_the_workspace_version_and_main() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree(&temp.path().join("tree"), COMMIT);
        git(&root, &["add", "--all"]);
        git(&root, &["commit", "--quiet", "-m", "release 1.2.3"]);
        let origin = temp.path().join("origin.git");
        git(
            temp.path(),
            &["init", "--quiet", "--bare", origin.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&root, &["push", "--quiet", "origin", "HEAD:main"]);
        // A shallow clone of main at the release, as actions/checkout makes one.
        let checkout = temp.path().join("checkout");
        git(
            temp.path(),
            &[
                "clone",
                "--quiet",
                "--depth",
                "1",
                &format!("file://{}", origin.display()),
                checkout.to_str().unwrap(),
            ],
        );
        check_tag(&checkout, "v1.2.3").unwrap();
        assert_eq!(
            git(&checkout, &["rev-parse", "--is-shallow-repository"]).trim(),
            "false",
            "the check fetched main's history"
        );
        check_tag(&root, "v1.2.3").unwrap();
        assert_eq!(
            check_tag(&root, "v1.2.4").unwrap_err().to_string(),
            "the tag v1.2.4 does not release the workspace's version 1.2.3: bump \
             [workspace.package] version in Cargo.toml and tag that commit v1.2.3",
            "a tag for another version is refused"
        );
        assert!(check_tag(&root, "1.2.3").is_err(), "the tag needs its v");

        git(
            &root,
            &["commit", "--quiet", "--allow-empty", "-m", "unreviewed"],
        );
        let head = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(
            check_tag(&root, "v1.2.3").unwrap_err().to_string(),
            format!(
                "the tag v1.2.3 names {}, which is not on origin's main: tag a commit of main",
                head.trim()
            ),
            "a commit main does not hold is refused"
        );

        fs::write(
            root.join("Cargo.toml"),
            "[workspace.package]\nversion = \"1.2.3-rc.1\"\n",
        )
        .unwrap();
        assert_eq!(
            check_tag(&root, "v1.2.3-rc.1").unwrap_err().to_string(),
            "the workspace's version 1.2.3-rc.1 is not a release number such as 1.2.3",
            "a release is a release number"
        );
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        assert!(
            format!("{:#}", check_tag(&root, "v1.2.3").unwrap_err())
                .contains("Cargo.toml has no [workspace.package] version"),
            "a workspace without a version"
        );
    }

    #[test]
    fn finds_the_commit_a_tag_names() {
        let listed = "1111111111111111111111111111111111111111\trefs/tags/v1.0.0\n\
                      2222222222222222222222222222222222222222\trefs/tags/v1.0.0^{}\n";
        assert_eq!(
            tagged_commit(listed, "v1.0.0").as_deref(),
            Some("2222222222222222222222222222222222222222"),
            "an annotated tag's commit, not the tag object"
        );
        assert_eq!(
            tagged_commit(
                "3333333333333333333333333333333333333333\trefs/tags/v1.0.0\n",
                "v1.0.0"
            )
            .as_deref(),
            Some("3333333333333333333333333333333333333333"),
            "a lightweight tag names its commit itself"
        );
        assert_eq!(
            tagged_commit(listed, "v1.0"),
            None,
            "only the tag asked for"
        );
    }

    #[test]
    fn fetches_tailscale_at_the_pinned_tag_and_commit() {
        let temp = tempfile::tempdir().unwrap();
        let upstream = repository(&temp.path().join("upstream"));
        fs::write(upstream.join("go.mod"), "module tailscale.com\n").unwrap();
        git(&upstream, &["add", "go.mod"]);
        git(&upstream, &["commit", "--quiet", "-m", "release"]);
        git(
            &upstream,
            &["tag", "--annotate", "-m", "v1.104.5", "v1.104.5"],
        );
        let released = git(&upstream, &["rev-parse", "HEAD"]).trim().to_owned();
        git(
            &upstream,
            &["commit", "--quiet", "--allow-empty", "-m", "later"],
        );
        let url = format!("file://{}", upstream.display());

        let root = tree(&temp.path().join("tree"), &released);
        let mut out = Vec::new();
        let source = tailscale_source(&root, &url, &mut out).unwrap();
        assert_eq!(source, root.join("target/tailscale"), "into target/");
        assert_eq!(
            git(&source, &["rev-parse", "HEAD"]).trim(),
            released,
            "the pinned commit, not the branch's tip"
        );
        assert!(source.join("go.mod").is_file(), "with its files");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "xtask release: fetched Tailscale v1.104.5 into {}\n",
                source.display()
            ),
            "says what it fetched"
        );

        let mut kept = Vec::new();
        tailscale_source(&root, &url, &mut kept).unwrap();
        assert_eq!(
            String::from_utf8(kept).unwrap(),
            format!(
                "xtask release: {} holds Tailscale v1.104.5\n",
                source.display()
            ),
            "a clean checkout at the commit is kept"
        );

        fs::write(source.join("go.mod"), "changed\n").unwrap();
        let mut refetched = Vec::new();
        tailscale_source(&root, &url, &mut refetched).unwrap();
        assert!(
            String::from_utf8(refetched).unwrap().contains("fetched"),
            "a changed checkout is fetched afresh"
        );
        assert_eq!(
            fs::read_to_string(source.join("go.mod")).unwrap(),
            "module tailscale.com\n",
            "with the pinned files"
        );

        let moved = tree(&temp.path().join("moved"), COMMIT);
        assert_eq!(
            format!(
                "{:#}",
                tailscale_source(&moved, &url, &mut Vec::new()).unwrap_err()
            ),
            format!(
                "{url}'s tag v1.104.5 names {released}, not the pinned {COMMIT}: check the \
                 release and update packaging/tailscale.toml"
            ),
            "a tag that names another commit is refused"
        );
        git(&upstream, &["tag", "--delete", "v1.104.5"]);
        assert_eq!(
            format!(
                "{:#}",
                tailscale_source(&root, &url, &mut Vec::new()).unwrap_err()
            ),
            format!("{url} has no tag v1.104.5"),
            "a missing tag is refused"
        );
    }

    #[test]
    fn reads_the_glibc_versions_a_binary_needs_from_its_version_table() {
        let binary = elf(
            &[
                ("libc.so.6", &["GLIBC_2.2.5", "GLIBC_2.28"]),
                ("libm.so.6", &["GLIBC_2.17"]),
                ("libgcc_s.so.1", &["GCC_3.0"]),
            ],
            b"a message that names GLIBC_2.99\0",
        );
        let found: Vec<Vec<u32>> = glibc_versions(&binary).unwrap().into_iter().collect();
        assert_eq!(
            found,
            [vec![2, 2, 5], vec![2, 17], vec![2, 28]],
            "every glibc version a library is needed at, and not GLIBC_2.99, which is only a \
             string"
        );
        within_glibc(&binary, Some([2, 28])).unwrap();
        assert_eq!(
            within_glibc(
                &linked(&["GLIBC_2.34", "GLIBC_2.28", "GLIBC_2.39"]),
                Some([2, 28])
            )
            .unwrap_err()
            .to_string(),
            "it needs glibc 2.34, 2.39, newer than the floor 2.28: build it in the release \
             workflow's AlmaLinux 8 container",
            "names the versions over the floor"
        );
        assert!(
            within_glibc(&linked(&["GLIBC_2.28.1"]), Some([2, 28])).is_err(),
            "a patch release is newer"
        );
        assert_eq!(
            format!(
                "{:#}",
                within_glibc(&linked(&["GLIBC_ABI_DT_RELR"]), Some([2, 28])).unwrap_err()
            ),
            "it needs GLIBC_ABI_DT_RELR, which is no glibc version",
            "a glibc need no version number accounts for"
        );
        let static_build = elf(&[], b"GLIBC_2.17\0");
        assert!(
            glibc_versions(&static_build).unwrap().is_empty(),
            "a static binary needs nothing"
        );
        within_glibc(&static_build, None).unwrap();
        assert_eq!(
            within_glibc(&linked(&["GLIBC_2.17"]), None)
                .unwrap_err()
                .to_string(),
            "a static build must need no glibc, but it needs 2.17",
            "a static build needs none at all"
        );
        assert_eq!(
            format!("{:#}", glibc_versions(b"GLIBC_2.17").unwrap_err()),
            "it is not a 64-bit ELF binary: Invalid ELF header size or alignment",
            "a file that is no ELF binary"
        );
    }

    /// The entries of the gzipped tarball at `path`: name, mode, mtime, uid, gid and contents.
    fn entries(path: &Path) -> Vec<(String, u32, u64, u64, u64, Vec<u8>)> {
        let file = fs::File::open(path).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        tar.entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let header = entry.header().clone();
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).unwrap();
                (
                    entry.path().unwrap().display().to_string(),
                    header.mode().unwrap(),
                    header.mtime().unwrap(),
                    header.uid().unwrap(),
                    header.gid().unwrap(),
                    contents,
                )
            })
            .collect()
    }

    #[test]
    fn packs_files_under_one_directory_owned_by_root() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("binary"), "program").unwrap();
        fs::write(temp.path().join("licence"), "text").unwrap();
        let path = temp.path().join("a.tar.gz");
        archive(
            &path,
            "a-1",
            &[
                ("a", temp.path().join("binary"), 0o755),
                ("LICENSE", temp.path().join("licence"), 0o644),
            ],
            1_700_000_000,
        )
        .unwrap();
        assert_eq!(
            entries(&path),
            [
                (
                    "a-1/a".to_owned(),
                    0o755,
                    1_700_000_000,
                    0,
                    0,
                    b"program".to_vec()
                ),
                (
                    "a-1/LICENSE".to_owned(),
                    0o644,
                    1_700_000_000,
                    0,
                    0,
                    b"text".to_vec()
                ),
            ],
            "each file with its mode, the commit's time and root as owner"
        );
        assert!(
            archive(
                &path,
                "a-1",
                &[("gone", temp.path().join("gone"), 0o644)],
                0
            )
            .is_err(),
            "a missing file fails"
        );
    }

    #[test]
    fn renders_the_template_for_every_target() {
        let template =
            fs::read_to_string(crate::testing::checkout().join(FORMULA_TEMPLATE)).unwrap();
        let digest = |archive: &str| Ok(format!("sha256-of-{archive}"));
        let rendered = render(
            &template,
            &versions(),
            "https://example.com/v1.2.3/",
            &digest,
        )
        .unwrap();
        assert!(!rendered.contains("@@"), "every field is filled in");
        assert!(rendered.contains("  version \"1.2.3\"\n"), "{rendered}");
        assert!(
            rendered.contains("shell_output(\"#{libexec}/tailscaled --version\")"),
            "the test runs the private tailscaled"
        );
        assert!(
            rendered.contains("assert_match \"1.104.5\""),
            "and expects Tailscale's version"
        );
        for target in Target::ALL {
            for archive in [
                format!("abc-1.2.3-{target}.tar.gz"),
                format!("tailscaled-1.104.5-{target}.tar.gz"),
            ] {
                assert!(
                    rendered.contains(&format!("url \"https://example.com/v1.2.3/{archive}\"\n")),
                    "{archive}'s URL, without a doubled slash"
                );
                assert!(
                    rendered.contains(&format!("sha256 \"sha256-of-{archive}\"\n")),
                    "{archive}'s sha256"
                );
            }
        }
        assert!(
            rendered.contains("libexec.install \"tailscaled\"")
                && rendered.contains("depends_on \"postgresql@18\"")
                && rendered.contains("refute_path_exists bin/\"tailscaled\""),
            "tailscaled stays in libexec, and Postgres 18 comes along"
        );
        assert!(
            rendered.contains(
                "libexec.install_symlink formula_opt_bin(\"postgresql@18\") => \"postgresql\""
            ) && rendered
                .contains("assert_predicate libexec/\"postgresql/postgres\", :executable?"),
            "abc finds the keg-only Postgres 18 in its own libexec"
        );
        assert!(
            rendered.contains("Never edit Formula/abc.rb in the tap by hand"),
            "the rendered formula says where to change it"
        );
    }

    #[test]
    fn refuses_a_template_with_unknown_missing_or_open_fields() {
        let digest = |_: &str| Ok("digest".to_owned());
        let full: String = ["url", "sha256"]
            .into_iter()
            .flat_map(|field| {
                ["abc", "tailscaled"].into_iter().flat_map(move |program| {
                    Target::ALL
                        .into_iter()
                        .map(move |target| format!("@@{field} {program} {target}@@\n"))
                })
            })
            .collect();
        let error = |template: &str| {
            format!(
                "{:#}",
                render(template, &versions(), "file:///dist", &digest).unwrap_err()
            )
        };
        let rendered = render(&full, &versions(), "file:///dist", &digest).unwrap();
        assert!(
            rendered.starts_with("file:///dist/abc-1.2.3-aarch64-apple-darwin.tar.gz\n"),
            "{rendered}"
        );
        assert_eq!(
            error(&format!("{full}@@colour@@")),
            "unknown field @@colour@@",
            "an unknown field"
        );
        assert_eq!(
            error(&format!("{full}@@url abc@@")),
            "unknown field @@url abc@@",
            "a URL without a target"
        );
        assert_eq!(
            error(&format!("{full}@@url gh aarch64-apple-darwin@@")),
            "no program gh in @@url gh aarch64-apple-darwin@@",
            "an unknown program"
        );
        assert!(
            error(&format!("{full}@@sha256 abc riscv64gc-unknown-linux-gnu@@"))
                .contains("not riscv64gc-unknown-linux-gnu"),
            "an unknown target"
        );
        assert_eq!(
            error(&format!("{full}@@version")),
            "an @@ field is not closed",
            "an open field"
        );
        assert_eq!(
            error(&full.replace("@@sha256 tailscaled x86_64-unknown-linux-gnu@@", "")),
            "the template lacks @@sha256 tailscaled x86_64-unknown-linux-gnu@@",
            "a missing field"
        );
        let failing =
            |archive: &str| -> anyhow::Result<String> { anyhow::bail!("{archive} is missing") };
        assert_eq!(
            format!(
                "{:#}",
                render(&full, &versions(), "file:///dist", &failing).unwrap_err()
            ),
            "abc-1.2.3-aarch64-apple-darwin.tar.gz is missing",
            "a digest that cannot be had"
        );
    }

    #[test]
    fn renders_the_formula_for_the_archives_in_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree(&temp.path().join("tree"), COMMIT);
        let dist = temp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        for target in Target::ALL {
            for program in Program::ALL {
                let name = versions().archive(program, target);
                fs::write(dist.join(&name), &name).unwrap();
            }
        }
        let rendered = formula(&root, &dist, "file:///dist").unwrap();
        let name = "abc-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        assert!(
            rendered.contains(&format!(
                "sha256 \"{}\"",
                crate::tools::sha256_hex(name.as_bytes())
            )),
            "each archive's own sha256"
        );
        fs::remove_file(dist.join(name)).unwrap();
        let error = format!("{:#}", formula(&root, &dist, "file:///dist").unwrap_err());
        assert!(
            error.starts_with(&format!(
                "could not render packaging/homebrew/abc.rb: {name} is missing from {}: \
                 download every target's archives first",
                dist.display()
            )),
            "{error}"
        );
        fs::remove_file(root.join(FORMULA_TEMPLATE)).unwrap();
        assert_eq!(
            formula(&root, &dist, "file:///dist")
                .unwrap_err()
                .to_string(),
            "could not read packaging/homebrew/abc.rb",
            "without a template"
        );
    }

    #[test]
    fn the_checked_in_pin_and_template_render() {
        let root = crate::testing::checkout();
        let versions = Versions::read(&root).unwrap();
        let template = fs::read_to_string(root.join(FORMULA_TEMPLATE)).unwrap();
        render(
            &template,
            &versions,
            "file:///dist",
            &|_| Ok("0".repeat(64)),
        )
        .unwrap();
    }

    /// A command as the fake runner saw it: program, arguments, directory and environment.
    #[derive(Debug)]
    struct Ran {
        program: String,
        args: Vec<String>,
        dir: Option<PathBuf>,
        env: Vec<(String, Option<String>)>,
    }

    impl Ran {
        fn of(command: &Command) -> Self {
            let text = |value: &std::ffi::OsStr| value.to_string_lossy().into_owned();
            Self {
                program: text(command.get_program()),
                args: command.get_args().map(text).collect(),
                dir: command.get_current_dir().map(Path::to_path_buf),
                env: command
                    .get_envs()
                    .map(|(name, value)| (text(name), value.map(text)))
                    .collect(),
            }
        }

        fn env(&self, name: &str) -> Option<&str> {
            self.env
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.as_deref())
        }
    }

    /// Packages `target` in a tree with Tailscale's source, running every command with a fake
    /// that records it, writes the binaries a build would (abc with `abc`'s bytes) and fails the
    /// command whose program is `fail`. Returns whether it passed, what it said and what ran.
    fn packaged(root: &Path, target: Target, abc: &[u8], fail: &str) -> (bool, String, Vec<Ran>) {
        let ran = RefCell::new(Vec::new());
        let mut run = |command: &mut Command| -> anyhow::Result<bool> {
            let seen = Ran::of(command);
            let passed = seen.program.rsplit('/').next() != Some(fail);
            let go = seen.program.ends_with("/target/go/bin/go");
            if passed && seen.args.first().map(String::as_str) == Some("build") {
                let output = if go {
                    PathBuf::from(&seen.args[5])
                } else {
                    root.join("target")
                        .join(target.triple())
                        .join("release/abc")
                };
                fs::create_dir_all(output.parent().unwrap()).unwrap();
                fs::write(
                    output,
                    if go {
                        elf(&[], b"tailscaled")
                    } else {
                        abc.to_vec()
                    },
                )
                .unwrap();
            }
            ran.borrow_mut().push(seen);
            Ok(passed)
        };
        let mut out = Vec::new();
        let passed = package(root, target, &mut run, &mut out).unwrap();
        (passed, String::from_utf8(out).unwrap(), ran.into_inner())
    }

    /// A tree with Tailscale's source in target/tailscale.
    fn tree_with_source(dir: &Path) -> PathBuf {
        let root = tree(dir, COMMIT);
        fs::create_dir_all(root.join("web")).unwrap();
        fs::write(root.join("web/package.json"), "{}\n").unwrap();
        let source = root.join("target/tailscale");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("go.mod"), "module tailscale.com\n\ngo 1.27.3\n").unwrap();
        fs::write(source.join("LICENSE"), "BSD-3-Clause\n").unwrap();
        fs::create_dir_all(root.join("target/go")).unwrap();
        fs::write(
            root.join("target/go/VERSION"),
            "go1.27.3\ntime 2026-10-07T00:00:00Z\n",
        )
        .unwrap();
        fs::write(source.join("PATENTS"), "patent grant\n").unwrap();
        // The web app, built and packed once for HEAD; packaging puts it back in place.
        let toolchain = fake_web(&root.join("../tools"), "");
        let mut said = Vec::new();
        assert!(
            pack_web(&root, &toolchain, &mut crate::web::run_here, &mut said).unwrap(),
            "{}",
            String::from_utf8_lossy(&said)
        );
        fs::remove_dir_all(root.join("web/dist")).unwrap();
        root
    }

    #[test]
    fn packages_abc_and_tailscaled_for_linux() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        let (passed, said, ran) = packaged(&root, Target::LinuxArm, &linked(&["GLIBC_2.28"]), "");
        assert!(passed, "{said}");
        let programs: Vec<(&str, &[String])> = ran
            .iter()
            .map(|ran| (ran.program.rsplit('/').next().unwrap(), ran.args.as_slice()))
            .collect();
        assert_eq!(
            programs
                .iter()
                .map(|(program, _)| *program)
                .collect::<Vec<_>>(),
            ["rustup", "cargo", "go"],
            "the target, abc and tailscaled, and no node"
        );
        assert_eq!(
            ran[0].args,
            ["target", "add", "aarch64-unknown-linux-gnu"],
            "the target's standard library"
        );
        assert_eq!(
            ran[0].dir.as_deref(),
            Some(root.as_path()),
            "for the pinned toolchain"
        );
        assert_eq!(
            ran[1].args,
            [
                "build",
                "--release",
                "--locked",
                "-p",
                "abc",
                "--target",
                "aarch64-unknown-linux-gnu",
                "--target-dir",
                &root.join("target").display().to_string()
            ],
            "a locked release build for the target"
        );
        assert_eq!(ran[1].dir.as_deref(), Some(root.as_path()), "in the tree");
        assert_eq!(
            ran[1].env("MACOSX_DEPLOYMENT_TARGET"),
            None,
            "no macOS floor on Linux"
        );
        assert!(
            ran[1].env.contains(&("CARGO_TARGET_DIR".to_owned(), None)),
            "the tree's own target directory"
        );
        let go = &ran[2];
        assert_eq!(
            go.args[..4],
            [
                "build",
                "-trimpath",
                "-ldflags",
                &versions().tailscale.ldflags()
            ],
            "a stamped, reproducible build"
        );
        assert_eq!(
            go.args[4..],
            [
                "-o",
                &root
                    .join("target/aarch64-unknown-linux-gnu/tailscaled")
                    .display()
                    .to_string(),
                "./cmd/tailscaled"
            ],
            "of tailscaled, beside abc's build"
        );
        assert_eq!(
            go.dir.as_deref(),
            Some(root.join("target/tailscale").as_path()),
            "in Tailscale's source"
        );
        for (name, value) in [
            ("GOOS", "linux"),
            ("GOARCH", "arm64"),
            ("GOTOOLCHAIN", "local"),
            ("GOFLAGS", "-mod=readonly"),
            ("CGO_ENABLED", "0"),
        ] {
            assert_eq!(go.env(name), Some(value), "{name}");
        }
        assert_eq!(go.env("CC"), None, "a static build needs no C compiler");
    }

    #[test]
    fn packs_each_program_with_its_licences() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        let (passed, said, _) = packaged(&root, Target::LinuxArm, &linked(&["GLIBC_2.28"]), "");
        assert!(passed, "{said}");
        let dist = root.join("target/dist");
        let time: u64 = git(&root, &["log", "-1", "--format=%ct"])
            .trim()
            .parse()
            .unwrap();
        let abc = entries(&dist.join("abc-1.2.3-aarch64-unknown-linux-gnu.tar.gz"));
        assert_eq!(
            abc,
            [
                (
                    "abc-1.2.3-aarch64-unknown-linux-gnu/abc".to_owned(),
                    0o755,
                    time,
                    0,
                    0,
                    linked(&["GLIBC_2.28"])
                ),
                (
                    "abc-1.2.3-aarch64-unknown-linux-gnu/LICENSE".to_owned(),
                    0o644,
                    time,
                    0,
                    0,
                    b"MIT licence\n".to_vec()
                ),
            ],
            "abc with its licence, dated by the commit"
        );
        let tailscaled: Vec<(String, Vec<u8>)> =
            entries(&dist.join("tailscaled-1.104.5-aarch64-unknown-linux-gnu.tar.gz"))
                .into_iter()
                .map(|(name, _, _, _, _, contents)| (name, contents))
                .collect();
        assert_eq!(
            tailscaled,
            [
                (
                    "tailscaled-1.104.5-aarch64-unknown-linux-gnu/tailscaled".to_owned(),
                    elf(&[], b"tailscaled")
                ),
                (
                    "tailscaled-1.104.5-aarch64-unknown-linux-gnu/LICENSE".to_owned(),
                    b"BSD-3-Clause\n".to_vec()
                ),
                (
                    "tailscaled-1.104.5-aarch64-unknown-linux-gnu/PATENTS".to_owned(),
                    b"patent grant\n".to_vec()
                ),
            ],
            "tailscaled with Tailscale's licence and patent grant"
        );
        assert!(
            said.ends_with(&format!(
                "xtask release: packaged {}\n",
                dist.join("tailscaled-1.104.5-aarch64-unknown-linux-gnu.tar.gz")
                    .display()
            )),
            "{said}"
        );
    }

    #[test]
    fn builds_for_an_older_macos_with_cgo() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        let (passed, said, ran) = packaged(&root, Target::AppleIntel, b"GLIBC_2.99", "");
        assert!(passed, "glibc is no matter on macOS: {said}");
        assert_eq!(
            ran[1].env("MACOSX_DEPLOYMENT_TARGET"),
            Some("12.0"),
            "abc runs on macOS 12"
        );
        let go = &ran[2];
        for (name, value) in [
            ("GOOS", "darwin"),
            ("GOARCH", "amd64"),
            ("CGO_ENABLED", "1"),
            ("CC", "clang"),
            ("CGO_CFLAGS", "-arch x86_64 -mmacosx-version-min=12.0"),
            ("CGO_LDFLAGS", "-arch x86_64 -mmacosx-version-min=12.0"),
        ] {
            assert_eq!(go.env(name), Some(value), "{name}");
        }
        let (_, _, arm) = packaged(&root, Target::AppleArm, b"", "");
        assert_eq!(
            arm[2].env("CGO_CFLAGS"),
            Some("-arch arm64 -mmacosx-version-min=12.0"),
            "arm64 keeps Go's name"
        );
        assert_eq!(arm[2].env("GOARCH"), Some("arm64"), "on Apple silicon");
    }

    #[test]
    fn refuses_a_linux_build_over_the_glibc_floor() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        let (passed, said, _) = packaged(&root, Target::LinuxIntel, &linked(&["GLIBC_2.34"]), "");
        assert!(!passed, "a build that needs a newer glibc");
        assert_eq!(
            said,
            format!(
                "xtask release: {}: it needs glibc 2.34, newer than the floor 2.28: build it in \
                 the release workflow's AlmaLinux 8 container\n",
                root.join("target/x86_64-unknown-linux-gnu/release/abc")
                    .display()
            ),
            "names the binary and the versions"
        );
        assert!(!root.join("target/dist").exists(), "and packages nothing");
    }

    #[test]
    fn packs_the_web_app_once_and_embeds_exactly_that_build() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        let head = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        let packed = root.join(format!("target/web/web-{head}.tar.gz"));
        let names: Vec<String> = entries(&packed)
            .into_iter()
            .map(|(name, mode, _, _, _, _)| format!("{name} {mode:o}"))
            .collect();
        assert_eq!(
            names,
            ["dist/index.html 644"],
            "web/dist's files, under dist/, for HEAD"
        );
        // A stale build in web/dist is replaced by the packed one.
        fs::create_dir_all(root.join("web/dist")).unwrap();
        fs::write(root.join("web/dist/stale.js"), "old").unwrap();
        let (passed, said, _) = packaged(&root, Target::AppleArm, b"", "");
        assert!(passed, "{said}");
        assert!(
            root.join("web/dist/index.html").is_file() && !root.join("web/dist/stale.js").exists(),
            "the binary embeds exactly the packed web app"
        );
        assert!(
            !root.join("target/web/unpacked").exists(),
            "leaving nothing beside it"
        );

        git(
            &root,
            &["commit", "--quiet", "--allow-empty", "-m", "later"],
        );
        let (later, later_said, ran) = packaged(&root, Target::AppleArm, b"", "");
        assert!(!later, "a web app packed at another commit");
        assert_eq!(
            later_said,
            format!(
                "xtask release: {} holds no web app built at HEAD: run `cargo xtask release \
                 web` first, on this commit (the release workflow's web job does, and every \
                 build downloads its archive)\n",
                root.join(format!(
                    "target/web/web-{}.tar.gz",
                    git(&root, &["rev-parse", "HEAD"]).trim()
                ))
                .display()
            ),
            "names the command that packs it"
        );
        assert!(ran.is_empty(), "before building anything");
    }

    #[test]
    fn a_failed_web_build_packs_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree(&temp.path().join("tree"), COMMIT);
        fs::create_dir_all(root.join("web")).unwrap();
        fs::write(root.join("web/package.json"), "{}\n").unwrap();
        let toolchain = fake_web(&temp.path().join("tools"), "build");
        let mut said = Vec::new();
        assert!(
            !pack_web(&root, &toolchain, &mut crate::web::run_here, &mut said).unwrap(),
            "the build fails"
        );
        assert!(
            String::from_utf8(said)
                .unwrap()
                .starts_with("xtask release: web build failed: "),
            "names the step"
        );
        assert!(!root.join("target/web").exists(), "and packs nothing");
    }

    #[test]
    fn stops_at_the_first_step_that_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        for (fail, said, steps) in [
            (
                "rustup",
                "xtask release: aarch64-apple-darwin: rustup could not add the target\n",
                1,
            ),
            (
                "cargo",
                "xtask release: aarch64-apple-darwin: cargo build failed: fix each error above\n",
                2,
            ),
            (
                "go",
                "xtask release: aarch64-apple-darwin: go build of tailscaled failed: fix each \
                 error above; the Go version is the one Tailscale's go.mod names\n",
                3,
            ),
        ] {
            let (passed, out, ran) = packaged(&root, Target::AppleArm, b"", fail);
            assert!(!passed, "{fail} fails");
            assert!(out.starts_with(said), "{fail}: {out}");
            assert_eq!(ran.len(), steps, "{fail}: nothing runs after it");
        }
        fs::remove_dir_all(root.join("target/tailscale")).unwrap();
        let (passed, out, ran) = packaged(&root, Target::AppleArm, b"", "");
        assert!(!passed, "no Tailscale source");
        assert_eq!(
            out,
            format!(
                "xtask release: {} holds no Tailscale source: run `cargo xtask release \
                 tailscale-source` first\n",
                root.join("target/tailscale").display()
            ),
            "names the command that fetches it"
        );
        assert!(ran.is_empty(), "before building anything");
    }

    #[test]
    fn refuses_to_build_without_the_pinned_go() {
        let temp = tempfile::tempdir().unwrap();
        let root = tree_with_source(&temp.path().join("tree"));
        fs::write(root.join("target/go/VERSION"), "go1.27.2\n").unwrap();
        let (passed, out, ran) = packaged(&root, Target::LinuxArm, &[], "");
        assert!(!passed, "another Go is installed");
        assert_eq!(
            out,
            format!(
                "xtask release: {} does not hold go1.27.3: run `cargo xtask release go` first\n",
                root.join("target/go").display()
            ),
            "names the command that installs it"
        );
        assert!(ran.is_empty(), "before building anything");
        fs::write(
            root.join("target/tailscale/go.mod"),
            "module tailscale.com\n\ngo 1.28.0\n",
        )
        .unwrap();
        let (moved, moved_out, _) = packaged(&root, Target::LinuxArm, &[], "");
        assert!(!moved, "Tailscale moved to another Go");
        assert_eq!(
            moved_out,
            "xtask release: Tailscale 1.104.5 builds with Go 1.28.0, but \
             packaging/tailscale.toml pins Go 1.27.3: pin Go 1.28.0 with the sha256s go.dev \
             publishes for it\n",
            "names the Go to pin"
        );
    }

    #[test]
    fn reads_the_go_version_a_go_mod_names() {
        assert_eq!(
            go_mod_version("module tailscale.com\n\ngo 1.27.3\n\nrequire (\n)\n").as_deref(),
            Some("1.27.3"),
            "the go directive"
        );
        assert_eq!(
            go_mod_version("module m\ngo 1.27\ntoolchain go1.27.3\n").as_deref(),
            Some("1.27.3"),
            "a toolchain directive decides"
        );
        assert_eq!(
            go_mod_version("module m\n// go 1.2\ngopher 1\n"),
            None,
            "only a directive"
        );
        assert_eq!(
            go_mod_version("go\n"),
            None,
            "a directive without a version"
        );
    }

    #[test]
    fn names_the_go_platform_of_each_release_runner() {
        for (os, arch, platform) in [
            ("macos", "aarch64", "darwin-arm64"),
            ("linux", "x86_64", "linux-amd64"),
            ("linux", "aarch64", "linux-arm64"),
        ] {
            assert_eq!(go_platform(os, arch).unwrap(), platform, "{os} on {arch}");
        }
        assert_eq!(
            go_platform("macos", "x86_64").unwrap_err().to_string(),
            "the release builds on darwin-arm64, linux-amd64, linux-arm64, not macos on x86_64",
            "Intel macOS builds nothing"
        );
    }

    /// A gzipped tarball holding `files` (path and contents).
    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(contents.len()).unwrap());
            header.set_mode(0o755);
            tar.append_data(&mut header, path, *contents).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn installs_the_pinned_go_once_its_archive_matches_go_devs_sha256() {
        let temp = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            ("go/VERSION", b"go1.27.3\ntime 2026-10-07T00:00:00Z\n"),
            ("go/bin/go", b"#!/bin/sh\n"),
        ]);
        let sha = crate::tools::sha256_hex(&archive);
        let root = tree(&temp.path().join("tree"), COMMIT);
        fs::write(root.join(TAILSCALE_PIN), pin_text(COMMIT, &sha)).unwrap();
        let fetched = RefCell::new(Vec::new());
        let mut fetch = |url: &str| {
            fetched.borrow_mut().push(url.to_owned());
            Ok(archive.clone())
        };
        let install = |source: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>| {
            let mut out = Vec::new();
            go(
                &root,
                ("linux", "aarch64"),
                "https://go.example/dl",
                source,
                &mut out,
            )
            .map(|()| String::from_utf8(out).unwrap())
            .map_err(|error| format!("{error:#}"))
        };
        assert_eq!(
            install(&mut fetch).unwrap_err(),
            format!(
                "could not read {}: run `cargo xtask release tailscale-source` first: No such \
                 file or directory (os error 2)",
                root.join("target/tailscale/go.mod").display()
            ),
            "Tailscale's source names the Go"
        );
        fs::create_dir_all(root.join("target/tailscale")).unwrap();
        fs::write(root.join("target/tailscale/go.mod"), "go 1.27.2\n").unwrap();
        assert!(
            install(&mut fetch)
                .unwrap_err()
                .starts_with("Tailscale 1.104.5 builds with Go 1.27.2, but"),
            "a go.mod that names another Go"
        );
        assert!(fetched.borrow().is_empty(), "downloads nothing then");

        fs::write(root.join("target/tailscale/go.mod"), "go 1.27.3\n").unwrap();
        fs::create_dir_all(root.join("target/go/stale")).unwrap();
        assert_eq!(
            install(&mut fetch).unwrap(),
            format!(
                "xtask release: installed Go 1.27.3 for linux-arm64 into {}\n",
                root.join("target/go").display()
            ),
            "installs it"
        );
        assert_eq!(
            fetched.borrow().as_slice(),
            ["https://go.example/dl/go1.27.3.linux-arm64.tar.gz"],
            "go.dev's archive for the platform"
        );
        assert!(
            !root.join("target/go/stale").exists(),
            "in place of whatever was there"
        );
        assert_eq!(
            fs::metadata(root.join("target/go/bin/go"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111,
            "with Go executable"
        );
        assert!(
            !root.join("target/go.unpacked").exists(),
            "leaving nothing beside it"
        );
    }

    #[test]
    fn refuses_a_go_archive_that_is_not_the_pinned_one() {
        let temp = tempfile::tempdir().unwrap();
        let archive = tarball(&[("go/VERSION", b"go1.27.3\n")]);
        let sha = crate::tools::sha256_hex(&archive);
        let root = tree(&temp.path().join("tree"), COMMIT);
        fs::write(root.join(TAILSCALE_PIN), pin_text(COMMIT, &sha)).unwrap();
        fs::create_dir_all(root.join("target/tailscale")).unwrap();
        fs::write(root.join("target/tailscale/go.mod"), "go 1.27.3\n").unwrap();
        let install = |source: &mut dyn FnMut(&str) -> anyhow::Result<Vec<u8>>| {
            go(
                &root,
                ("linux", "aarch64"),
                "https://go.example/dl",
                source,
                &mut Vec::new(),
            )
            .map_err(|error| format!("{error:#}"))
            .unwrap_err()
        };
        let mut tampered = |_: &str| Ok(tarball(&[("go/VERSION", b"go1.27.3\n\n")]));
        let refused = install(&mut tampered);
        assert!(
            refused.starts_with("https://go.example/dl/go1.27.3.linux-arm64.tar.gz has sha256 ")
                && refused.ends_with(&format!(
                    ", not the {sha} packaging/tailscale.toml pins from go.dev"
                )),
            "an archive that is not go.dev's: {refused}"
        );

        let other = tarball(&[("go/VERSION", b"go1.27.2\n")]);
        fs::write(
            root.join(TAILSCALE_PIN),
            pin_text(COMMIT, &crate::tools::sha256_hex(&other)),
        )
        .unwrap();
        assert_eq!(
            install(&mut |_: &str| Ok(other.clone())),
            format!(
                "{} does not hold go1.27.3: run `cargo xtask release go` first",
                root.join("target/go").display()
            ),
            "an archive of another Go"
        );
        let flat = tarball(&[("VERSION", b"go1.27.3\n")]);
        fs::write(
            root.join(TAILSCALE_PIN),
            pin_text(COMMIT, &crate::tools::sha256_hex(&flat)),
        )
        .unwrap();
        assert!(
            install(&mut |_: &str| Ok(flat.clone())).starts_with(
                "https://go.example/dl/go1.27.3.linux-arm64.tar.gz holds no go directory"
            ),
            "an archive without Go's directory"
        );
        assert!(
            install(&mut |_: &str| Ok(b"not gzip".to_vec())).contains("has sha256"),
            "anything else is refused by its sha256 first"
        );
    }

    /// GitHub's contents response for a tap formula at blob `sha`.
    fn contents(formula: &str, sha: &str) -> String {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(formula);
        // GitHub wraps the base64 at 60 characters.
        let wrapped: Vec<String> = encoded
            .as_bytes()
            .chunks(60)
            .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap())
            .collect();
        serde_json::json!({"sha": sha, "content": wrapped.join("\n"), "encoding": "base64"})
            .to_string()
    }

    #[test]
    fn plans_the_tap_update_and_never_moves_it_backwards() {
        let formula =
            |version: &str| format!("class Abc < Formula\n  version \"{version}\"\nend\n");
        let release = formula("0.10.0");
        assert_eq!(
            tap_plan(&release, None).unwrap(),
            TapPlan::Create,
            "a tap without the formula"
        );
        assert_eq!(
            tap_plan(&release, Some(&contents(&formula("0.9.3"), "blob"))).unwrap(),
            TapPlan::Update("blob".to_owned()),
            "an older release is replaced, by number and not by text"
        );
        assert_eq!(
            tap_plan(&release, Some(&contents(&release, "blob"))).unwrap(),
            TapPlan::Keep,
            "the same formula is kept"
        );
        assert_eq!(
            tap_plan(
                &release,
                Some(&contents(
                    &release.replace("end", "  # rebuilt\nend"),
                    "blob"
                ))
            )
            .unwrap(),
            TapPlan::Update("blob".to_owned()),
            "the same version rendered anew replaces it"
        );
        assert_eq!(
            tap_plan(&release, Some(&contents(&formula("0.10.1"), "blob")))
                .unwrap_err()
                .to_string(),
            "the tap holds abc 0.10.1, newer than this release's 0.10.0: a release never moves \
             the tap backwards",
            "a newer formula is never replaced"
        );
        for (plan, text) in [
            (TapPlan::Create, "create"),
            (TapPlan::Update("abc123".to_owned()), "update abc123"),
            (TapPlan::Keep, "keep"),
        ] {
            assert_eq!(plan.to_string(), text, "the tap job reads {text}");
        }
        let error = |rendered: &str, current: Option<&str>| {
            format!("{:#}", tap_plan(rendered, current).unwrap_err())
        };
        assert_eq!(
            error("class Abc < Formula\nend\n", None),
            "could not read the rendered formula: the formula declares no version",
            "a formula without a version"
        );
        assert_eq!(
            error(&release, Some(&contents(&formula("1.x"), "blob"))),
            "could not read the tap's formula: the formula's version \"1.x\" is not a release \
             number",
            "a tap formula without a release number"
        );
        assert!(
            error(&release, Some("{}"))
                .starts_with("the tap's formula is not a GitHub contents response"),
            "a response that is not the file's"
        );
        assert_eq!(
            error(
                &release,
                Some(r#"{"sha": "blob", "content": "", "encoding": "none"}"#)
            ),
            "GitHub gave the tap's formula in none, not base64",
            "a file too large for the contents API"
        );
        assert!(
            error(
                &release,
                Some(r#"{"sha": "blob", "content": "!!", "encoding": "base64"}"#)
            )
            .starts_with("the tap's formula is not base64"),
            "content that is not base64"
        );
        assert!(
            error(
                &release,
                Some(r#"{"sha": "blob", "content": "/w==", "encoding": "base64"}"#)
            )
            .starts_with("the tap's formula is not UTF-8"),
            "content that is not text"
        );
    }

    #[test]
    fn no_release_job_that_can_write_builds_anything() {
        let release = workflow_jobs(".github/workflows/release.yml");
        let mut writers = Vec::new();
        for (id, job) in release["jobs"].as_hash().unwrap() {
            let id = id.as_str().unwrap();
            let steps = job["steps"].as_vec().unwrap();
            let mentions_secret = steps.iter().any(|step| {
                step["env"].as_hash().is_some_and(|env| {
                    env.values().any(|value| {
                        value
                            .as_str()
                            .is_some_and(|value| value.contains("secrets."))
                    })
                })
            });
            let can_write = job["permissions"].as_hash().is_some_and(|permissions| {
                permissions
                    .values()
                    .any(|level| level.as_str() == Some("write"))
            });
            if !(can_write || mentions_secret) {
                continue;
            }
            writers.push(id.to_owned());
            for step in steps {
                let uses = step["uses"].as_str().unwrap_or_default();
                let action = uses.split('@').next().unwrap_or_default();
                assert!(
                    uses.is_empty()
                        || matches!(
                            action,
                            "actions/checkout"
                                | "actions/download-artifact"
                                | "actions/upload-artifact"
                                | "actions/attest-build-provenance"
                        ),
                    "{id} can write, so it runs no action but GitHub's own: {uses}"
                );
                let run = step["run"].as_str().unwrap_or_default();
                assert!(
                    !run.split_whitespace().any(|word| {
                        [
                            "cargo", "rustup", "pnpm", "npm", "npx", "go", "brew", "make", "curl",
                            "wget",
                        ]
                        .contains(&word)
                    }),
                    "{id} can write, so it builds and installs nothing: {run}"
                );
            }
        }
        assert_eq!(
            writers,
            ["attest", "release", "tap"],
            "the jobs that can write: attesting, publishing and the tap"
        );
    }

    /// What a run of `.github/release/publish.sh` did.
    struct Published {
        /// Whether it passed.
        passed: bool,
        /// What it said on stderr.
        said: String,
        /// The gh commands it ran after listing the releases, a line each.
        ran: String,
    }

    /// Runs `.github/release/publish.sh` for v1.2.3 on `files` (name and contents) with their
    /// SHA256SUMS, or with `sums` in its place when given, against a fake gh whose release
    /// listing prints `state`.
    fn publish(files: &[(&str, &[u8])], sums: Option<&str>, state: &str) -> Published {
        let temp = tempfile::tempdir().unwrap();
        let dist = temp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        let mut listed = String::new();
        for (name, contents) in files {
            fs::write(dist.join(name), contents).unwrap();
            writeln!(listed, "{}  {name}", crate::tools::sha256_hex(contents)).unwrap();
        }
        fs::write(dist.join("SHA256SUMS"), sums.unwrap_or(&listed)).unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(temp.path().join("state"), state).unwrap();
        crate::testing::script(
            &bin.join("gh"),
            "case \"$1\" in\n\
             api) echo \"TAG=$TAG $*\" > \"$LISTED\"; cat \"$STATE\" ;;\n\
             *) echo \"$*\" >> \"$LOG\" ;;\n\
             esac\n",
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let output = Command::new("sh")
            .arg(crate::testing::checkout().join(".github/release/publish.sh"))
            .args(["v1.2.3"])
            .arg(&dist)
            .env("PATH", path)
            .env("LC_ALL", "C")
            .env("GITHUB_REPOSITORY", "getabc-dev/abc")
            .env("STATE", temp.path().join("state"))
            .env("LISTED", temp.path().join("listed"))
            .env("LOG", temp.path().join("log"))
            .output()
            .unwrap();
        if let Ok(listing) = fs::read_to_string(temp.path().join("listed")) {
            assert_eq!(
                listing,
                "TAG=v1.2.3 api --paginate repos/getabc-dev/abc/releases --jq .[] | \
                 select(.tag_name == env.TAG) | \"release \\(.draft)\", (.assets[] | \
                 \"asset \\(.name) \\(.digest // \"none\")\")\n",
                "lists the tag's releases, drafts included"
            );
        }
        Published {
            passed: output.status.success(),
            said: String::from_utf8(output.stderr).unwrap(),
            ran: fs::read_to_string(temp.path().join("log")).unwrap_or_default(),
        }
    }

    /// The digest GitHub records for an asset with `contents`.
    fn digest(contents: &[u8]) -> String {
        format!("sha256:{}", crate::tools::sha256_hex(contents))
    }

    #[test]
    fn publishes_a_release_and_resumes_one_left_unfinished() {
        let files: [(&str, &[u8]); 2] =
            [("abc-1.2.3-a.tar.gz", b"a"), ("abc-1.2.3-b.tar.gz", b"b")];
        let mut listed = String::new();
        for (name, contents) in files {
            writeln!(listed, "{}  {name}", crate::tools::sha256_hex(contents)).unwrap();
        }
        let sums = digest(listed.as_bytes());
        let upload = |name: &str| format!("release upload v1.2.3 {name} --repo getabc-dev/abc\n");
        let published = "release edit v1.2.3 --repo getabc-dev/abc --draft=false\n";

        let created = publish(&files, None, "");
        assert!(created.passed, "{}", created.said);
        assert_eq!(
            created.ran,
            format!(
                "release create v1.2.3 --repo getabc-dev/abc --draft --verify-tag --title abc \
                 1.2.3 --generate-notes\n{}{}{}{published}",
                upload("SHA256SUMS"),
                upload("abc-1.2.3-a.tar.gz"),
                upload("abc-1.2.3-b.tar.gz")
            ),
            "a new release is a draft until every asset is up"
        );

        let resumed = publish(
            &files,
            None,
            &format!(
                "release true\nasset SHA256SUMS {sums}\nasset abc-1.2.3-a.tar.gz {}\n",
                digest(b"a")
            ),
        );
        assert!(resumed.passed, "{}", resumed.said);
        assert_eq!(
            resumed.ran,
            format!("{}{published}", upload("abc-1.2.3-b.tar.gz")),
            "a draft gets only the assets it lacks, then is published"
        );

        let complete = format!(
            "release false\nasset SHA256SUMS {sums}\nasset abc-1.2.3-a.tar.gz {}\n\
             asset abc-1.2.3-b.tar.gz {}\n",
            digest(b"a"),
            digest(b"b")
        );
        let kept = publish(&files, None, &complete);
        assert!(kept.passed, "{}", kept.said);
        assert_eq!(kept.ran, "", "a complete release is left as it is");

        for (state, error) in [
            (
                complete.replace(&digest(b"b"), &digest(b"rebuilt")),
                format!(
                    "the release v1.2.3 holds abc-1.2.3-b.tar.gz with digest {}, not this run's \
                     {}: a published asset is never replaced",
                    digest(b"rebuilt"),
                    digest(b"b")
                ),
            ),
            (
                complete.replace(&digest(b"b"), "none"),
                "the release v1.2.3 holds abc-1.2.3-b.tar.gz with digest none".to_owned(),
            ),
            (
                format!("{complete}asset abc-1.2.3-c.tar.gz {}\n", digest(b"c")),
                "the release v1.2.3 holds abc-1.2.3-c.tar.gz, which this run did not build\n"
                    .to_owned(),
            ),
            (
                "release true\nrelease false\n".to_owned(),
                "getabc-dev/abc has 2 releases for v1.2.3: delete the drafts that are not the \
                 release\n"
                    .to_owned(),
            ),
        ] {
            let refused = publish(&files, None, &state);
            assert!(!refused.passed, "{error}");
            assert!(refused.said.starts_with(&error), "{}", refused.said);
            assert_eq!(refused.ran, "", "{error}: changes nothing");
        }
    }

    #[test]
    fn publishes_only_the_files_sha256sums_lists() {
        let files: [(&str, &[u8]); 2] =
            [("abc-1.2.3-a.tar.gz", b"a"), ("abc-1.2.3-b.tar.gz", b"b")];
        let mut listed = String::new();
        for (name, contents) in files {
            writeln!(listed, "{}  {name}", crate::tools::sha256_hex(contents)).unwrap();
        }
        let without_a: String = listed
            .lines()
            .filter(|line| !line.ends_with("a.tar.gz"))
            .flat_map(|line| [line, "\n"])
            .collect();
        let unlisted = publish(&files, Some(&without_a), "");
        assert!(!unlisted.passed, "an archive SHA256SUMS does not list");
        assert!(
            unlisted
                .said
                .ends_with("SHA256SUMS does not list abc-1.2.3-a.tar.gz\n"),
            "{}",
            unlisted.said
        );
        assert_eq!(unlisted.ran, "", "changes nothing");
        let tampered = publish(&[("abc-1.2.3-a.tar.gz", b"tampered")], Some(&listed), "");
        assert!(
            !tampered.passed,
            "an archive that is not the one SHA256SUMS lists"
        );
        assert_eq!(tampered.ran, "", "changes nothing");
    }

    /// The jobs of the workflow at `file`, relative to the repository root.
    fn workflow_jobs(file: &str) -> yaml_rust2::Yaml {
        let text = fs::read_to_string(crate::testing::checkout().join(file)).unwrap();
        yaml_rust2::YamlLoader::load_from_str(&text).unwrap()[0].clone()
    }

    /// `job` without the steps `drop` picks.
    fn without(
        job: &yaml_rust2::Yaml,
        drop: &dyn Fn(&yaml_rust2::Yaml) -> bool,
    ) -> yaml_rust2::Yaml {
        let mut job = job.as_hash().unwrap().clone();
        let steps = yaml_rust2::Yaml::String("steps".to_owned());
        let kept: Vec<yaml_rust2::Yaml> = job[&steps]
            .as_vec()
            .unwrap()
            .iter()
            .filter(|step| !drop(step))
            .cloned()
            .collect();
        job.insert(steps, yaml_rust2::Yaml::Array(kept));
        yaml_rust2::Yaml::Hash(job)
    }

    /// Whether `step` restores a cache: a cache action, a step told to cache, or a setup action
    /// that caches unless told not to, as `actions/setup-go` does, and `actions/setup-node`
    /// whenever package.json names a package manager.
    fn caches(step: &yaml_rust2::Yaml) -> bool {
        let uses = step["uses"].as_str().unwrap_or_default();
        let action = uses.split('@').next().unwrap_or_default();
        let told = &step["with"]["cache"];
        let caching_by_default = matches!(action, "actions/setup-go" | "actions/setup-node");
        uses.contains("cache@")
            || (told.is_badvalue() && caching_by_default)
            || (!told.is_badvalue() && told.as_bool() != Some(false))
    }

    #[test]
    fn finds_every_step_that_restores_a_cache() {
        let steps = yaml_rust2::YamlLoader::load_from_str(
            "- uses: Swatinem/rust-cache@0\n\
             - uses: actions/cache@0\n\
             - uses: actions/setup-go@0\n\
             - uses: actions/setup-node@0\n  with:\n    node-version: 24\n\
             - uses: actions/setup-go@0\n  with:\n    cache: true\n\
             - uses: actions/setup-python@0\n  with:\n    cache: pip\n\
             - uses: actions/setup-go@0\n  with:\n    cache: false\n\
             - uses: actions/setup-python@0\n\
             - run: cargo xtask release go\n",
        )
        .unwrap()[0]
            .clone();
        let found: Vec<bool> = steps.as_vec().unwrap().iter().map(caches).collect();
        assert_eq!(
            found,
            [true, true, true, true, true, true, false, false, false],
            "setup-go caches unless told not to"
        );
    }

    #[test]
    fn a_release_builds_and_installs_as_its_dry_run_does_without_caches() {
        let release = workflow_jobs(".github/workflows/release.yml");
        let dry_run = workflow_jobs(".github/workflows/release-dry-run.yml");
        let cache = |step: &yaml_rust2::Yaml| caches(step);
        let tag_check = |step: &yaml_rust2::Yaml| {
            step["run"]
                .as_str()
                .is_some_and(|run| run.starts_with("cargo xtask release check-tag "))
        };
        assert_eq!(
            release["on"],
            yaml_rust2::YamlLoader::load_from_str("push:\n  tags: [\"v*\"]\n").unwrap()[0],
            "a release runs on a tag alone"
        );
        // The dry run builds only when the change touches what a release builds from: its web
        // job, which every other job needs, waits for the inputs.
        let mut gated = dry_run["jobs"]["web"].as_hash().unwrap().clone();
        for (key, value) in [
            ("needs", "inputs"),
            ("if", "needs.inputs.outputs.changed == 'true'"),
        ] {
            assert_eq!(
                gated.remove(&yaml_rust2::Yaml::String(key.to_owned())),
                Some(yaml_rust2::Yaml::String(value.to_owned())),
                "the dry run's web job is gated on its inputs by {key}"
            );
        }
        let gated = yaml_rust2::Yaml::Hash(gated);
        for (id, cached) in [("web", 1), ("build", 1), ("formula", 1), ("install", 0)] {
            let job = &release["jobs"][id];
            let dry_job = if id == "web" {
                &gated
            } else {
                &dry_run["jobs"][id]
            };
            let steps = job["steps"].as_vec().unwrap();
            assert!(
                !steps.iter().any(cache),
                "release.yml's {id} restores no cache, so no published build reads one"
            );
            assert_eq!(
                dry_job["steps"]
                    .as_vec()
                    .unwrap()
                    .iter()
                    .filter(|step| cache(step))
                    .count(),
                cached,
                "the dry run's {id} caches its Rust builds"
            );
            assert_eq!(
                without(job, &tag_check),
                without(dry_job, &cache),
                "release.yml's {id} is the dry run's, without its caches and with the tag check"
            );
        }
        for workflow in [&release, &dry_run] {
            for step in workflow["jobs"]["build"]["steps"].as_vec().unwrap() {
                let run = step["run"].as_str().unwrap_or_default();
                assert!(
                    !["node", "pnpm", "release web"]
                        .iter()
                        .any(|word| run.contains(word)),
                    "no target's build runs node, which the AlmaLinux 8 container cannot: {run}"
                );
            }
        }
        assert_eq!(
            release["concurrency"]["group"].as_str(),
            Some("${{ github.workflow }}"),
            "releases queue across tags"
        );
        assert_eq!(
            release["concurrency"]["cancel-in-progress"].as_bool(),
            Some(false),
            "and never cancel one that runs"
        );
        assert_eq!(
            release["jobs"]["build"]["steps"]
                .as_vec()
                .unwrap()
                .iter()
                .filter(|step| tag_check(step))
                .count(),
            1,
            "a release checks its tag"
        );
    }
}
