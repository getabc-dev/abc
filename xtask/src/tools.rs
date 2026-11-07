//! The pinned tools in `xtask/tools.toml`: installing them into `.tools/`, checking them, and
//! running them from there. Nothing here runs a tool from `PATH`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};

use anyhow::{Context, bail, ensure};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

/// The tool list, relative to the repository root.
const MANIFEST: &str = "xtask/tools.toml";

/// Where tools are installed, relative to the repository root.
const TOOLS_DIR: &str = ".tools";

/// The largest archive `setup` and `pin` download: 256 MiB.
const MAX_ARCHIVE_BYTES: u64 = 268_435_456;

/// The platforms abc supports, each with its own release asset per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub(crate) enum Target {
    #[serde(rename = "aarch64-apple-darwin")]
    MacArm64,
    #[serde(rename = "x86_64-unknown-linux-gnu")]
    LinuxX64,
    #[serde(rename = "aarch64-unknown-linux-gnu")]
    LinuxArm64,
}

impl Target {
    const ALL: [Self; 3] = [Self::MacArm64, Self::LinuxX64, Self::LinuxArm64];

    const fn triple(self) -> &'static str {
        match self {
            Self::MacArm64 => "aarch64-apple-darwin",
            Self::LinuxX64 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
        }
    }

    /// The target of the machine xtask runs on.
    fn host() -> anyhow::Result<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Self::MacArm64),
            ("linux", "x86_64") => Ok(Self::LinuxX64),
            ("linux", "aarch64") => Ok(Self::LinuxArm64),
            (os, arch) => bail!(
                "abc pins tools for {} only, not for {arch} {os}",
                Self::ALL.map(Self::triple).join(", ")
            ),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.triple())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    tool: Vec<Tool>,
}

/// One pinned tool: a GitHub release asset per target, or a crate built from crates.io where a
/// project publishes no verifiable build for every target.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tool {
    name: String,
    version: String,
    /// Download URL template; `{version}` and `{platform}` are filled in.
    url: Option<String>,
    /// Path of the executable inside the `.tar.gz` archive, with the same placeholders; absent
    /// when the downloaded asset is the executable itself.
    binary: Option<String>,
    /// URL template of the checksums the project publishes for its assets, if it publishes any.
    checksums: Option<String>,
    /// The GitHub repository whose build attestations must list each archive's sha256, as
    /// `owner/repo`, for projects that attest their releases.
    attestations: Option<String>,
    /// URL template of the GitHub API's view of the release, whose asset digests (the sha256
    /// GitHub records as each asset is uploaded) must list each archive's sha256; for projects
    /// that publish neither checksums nor attestations.
    #[serde(rename = "release-digests")]
    release_digests: Option<String>,
    #[serde(default)]
    targets: BTreeMap<Target, Asset>,
    /// The crates.io crate to build the tool from, instead of release assets.
    #[serde(rename = "crate")]
    crate_name: Option<String>,
    /// Lowercase hex sha256 of the `.crate` file, as the crates.io index records it.
    sha256: Option<String>,
    /// For a crate: the features to build it with, instead of its default features.
    features: Option<Vec<String>>,
    /// The arguments that make the executable print its version, for cargo subcommands that
    /// expect their own name first. `--version` when absent.
    #[serde(rename = "version-args")]
    version_args: Option<Vec<String>>,
    /// Whether the tests run the tool, so every CI job that runs tests installs it with
    /// `cargo xtask setup --for-tests`.
    #[serde(rename = "for-tests", default)]
    pub(crate) for_tests: bool,
    /// Whether the whole archive is installed as the tool's directory, for a tool that needs the
    /// files beside its executable at run time, such as pnpm's `dist/`. `binary` is then the
    /// executable's path in that directory, and each target's `tree-sha256` pins the directory.
    #[serde(default)]
    tree: bool,
}

/// Where a tool comes from.
enum Source<'a> {
    Release { url: &'a str },
    Crate { name: &'a str, sha256: &'a str },
}

/// The crates.io index file listing every version of `name`.
fn index_url(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    let prefix = match (name.len(), name.get(..1), name.get(..2), name.get(2..4)) {
        (1, _, _, _) => "1".to_owned(),
        (2, _, _, _) => "2".to_owned(),
        (3, Some(first), _, _) => format!("3/{first}"),
        (_, _, Some(first), Some(second)) => format!("{first}/{second}"),
        _ => String::new(),
    };
    format!("https://index.crates.io/{prefix}/{name}")
}

/// One version's entry in the crates.io index.
#[derive(Deserialize)]
struct IndexEntry {
    vers: String,
    cksum: String,
}

/// A tool's release asset for one target.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Asset {
    /// The target's name in the project's asset file names.
    platform: String,
    /// Lowercase hex sha256 of the downloaded archive.
    sha256: String,
    /// Lowercase hex sha256 of the installed executable, checked whenever xtask uses it.
    binary_sha256: String,
    /// A shared library the executable loads from beside itself, as its path inside the archive;
    /// it is installed next to the executable.
    library: Option<String>,
    /// Lowercase hex sha256 of that library, checked whenever xtask uses the tool.
    library_sha256: Option<String>,
    /// For a tool installed as a directory: lowercase hex sha256 of the directory's manifest
    /// ([`Tree::manifest`]), every file with its sha256 and whether it is executable, checked
    /// whenever xtask uses the tool.
    tree_sha256: Option<String>,
}

/// The validated tool list.
#[derive(Debug)]
pub(crate) struct Manifest {
    tools: Vec<Tool>,
}

impl Manifest {
    pub(crate) fn load(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(MANIFEST);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("{MANIFEST} is invalid"))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: RawManifest = toml_edit::de::from_str(text)?;
        let mut names = BTreeSet::new();
        for tool in &raw.tool {
            ensure!(
                names.insert(tool.name.as_str()),
                "{} is listed twice",
                tool.name
            );
            tool.validate()
                .with_context(|| format!("tool {:?}", tool.name))?;
        }
        Ok(Self { tools: raw.tool })
    }

    /// The tools named in `only` and, with `for_tests`, those the tests run; all of them when
    /// neither names any.
    fn select(self, only: &[String], for_tests: bool) -> anyhow::Result<Self> {
        if only.is_empty() && !for_tests {
            return Ok(self);
        }
        for name in only {
            self.tool(name)?;
        }
        Ok(Self {
            tools: self
                .tools
                .into_iter()
                .filter(|tool| only.contains(&tool.name) || (for_tests && tool.for_tests))
                .collect(),
        })
    }

    pub(crate) fn tool(&self, name: &str) -> anyhow::Result<&Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name == name)
            .with_context(|| {
                let names: Vec<&str> = self.tools.iter().map(|tool| tool.name.as_str()).collect();
                format!(
                    "{name} is not a pinned tool; pinned tools: {}",
                    names.join(", ")
                )
            })
    }
}

impl Tool {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            is_safe_name(&self.name),
            "the name must be ASCII letters, digits, '-' or '_'"
        );
        ensure!(
            !self.version.is_empty()
                && self
                    .version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".+-".contains(c)),
            "version {:?} must be ASCII letters, digits, '.', '+' or '-'",
            self.version
        );
        if let Source::Crate { name, sha256 } = self.source()? {
            ensure!(
                name.split('-').all(is_safe_name),
                "crate {name:?} must be a crates.io crate name"
            );
            ensure!(
                is_sha256_hex(sha256),
                "sha256 must be 64 lowercase hex digits"
            );
            return Ok(());
        }
        ensure!(
            self.url
                .as_deref()
                .is_some_and(|url| url.starts_with("https://")),
            "the url must use https"
        );
        self.validate_vouchers()?;
        for target in Target::ALL {
            let asset = self.asset(target)?;
            ensure!(
                asset.platform.split('.').all(is_safe_name),
                "{target}: platform {:?} must be ASCII letters, digits, '-' or '_', in parts \
                 joined by '.'",
                asset.platform
            );
            for (field, value) in [
                ("sha256", &asset.sha256),
                ("binary-sha256", &asset.binary_sha256),
            ] {
                ensure!(
                    is_sha256_hex(value),
                    "{target}: {field} must be 64 lowercase hex digits"
                );
            }
            let url = self.render(self.url.as_deref().unwrap_or_default(), asset);
            let binary = self
                .binary
                .as_ref()
                .map(|template| self.render(template, asset))
                .unwrap_or_default();
            let render = |template: &Option<String>| {
                template
                    .as_ref()
                    .map(|template| self.render(template, asset))
                    .unwrap_or_default()
            };
            let checksums = render(&self.checksums);
            let release = render(&self.release_digests);
            for (field, value) in [
                ("url", &url),
                ("binary", &binary),
                ("checksums", &checksums),
                ("release-digests", &release),
            ] {
                ensure!(
                    !value.contains(['{', '}']),
                    "{target}: {field} {value:?} has a placeholder other than {{version}} and {{platform}}"
                );
            }
            ensure!(
                Path::new(&binary)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "{target}: binary {binary:?} must be a relative path inside the archive"
            );
            ensure!(
                self.binary.is_none() || !binary.is_empty(),
                "{target}: binary must not be empty; omit it when the asset is the executable"
            );
            self.validate_library(target, asset)?;
            self.validate_tree(target, asset)?;
        }
        Ok(())
    }

    /// Checks that a tool installed as a directory pins each target's tree, and that only such a
    /// tool does.
    fn validate_tree(&self, target: Target, asset: &Asset) -> anyhow::Result<()> {
        match (self.tree, &asset.tree_sha256) {
            (false, None) => Ok(()),
            (true, Some(sha256)) => {
                ensure!(
                    self.binary.is_some(),
                    "{target}: a tree comes only from an archive, which needs `binary`"
                );
                ensure!(
                    asset.library.is_none(),
                    "{target}: a tree holds its libraries itself; drop `library`"
                );
                ensure!(
                    is_sha256_hex(sha256),
                    "{target}: tree-sha256 must be 64 lowercase hex digits"
                );
                Ok(())
            }
            (true, None) => bail!("{target}: `tree = true` needs a tree-sha256"),
            (false, Some(_)) => bail!("{target}: tree-sha256 needs `tree = true`"),
        }
    }

    /// Checks what vouches for a release tool's downloads: at least one source, each well formed.
    fn validate_vouchers(&self) -> anyhow::Result<()> {
        ensure!(
            self.checksums.is_some()
                || self.attestations.is_some()
                || self.release_digests.is_some(),
            "needs `checksums`, `attestations` or `release-digests`: no tool's pin may rest on its \
             first download"
        );
        if let Some(release) = &self.release_digests {
            ensure!(
                release.starts_with("https://api.github.com/repos/"),
                "release-digests must be a release in the GitHub API, https://api.github.com/repos/..."
            );
        }
        if let Some(repository) = &self.attestations {
            ensure!(
                repository.split_once('/').is_some_and(
                    |(owner, repo)| is_safe_name(owner) && repo.split('.').all(is_safe_name)
                ),
                "attestations {repository:?} must be an owner/repo GitHub repository"
            );
        }
        if let Some(checksums) = &self.checksums {
            ensure!(
                checksums.starts_with("https://"),
                "the checksums url must use https"
            );
        }
        Ok(())
    }

    /// Checks the library of `asset`, the tool's asset for `target`, if it has one.
    fn validate_library(&self, target: Target, asset: &Asset) -> anyhow::Result<()> {
        match (self.library_member(asset), &asset.library_sha256) {
            (None, None) => Ok(()),
            (Some(library), Some(sha256)) => {
                ensure!(
                    self.binary.is_some(),
                    "{target}: a library comes only from an archive, which needs `binary`"
                );
                ensure!(
                    is_sha256_hex(sha256),
                    "{target}: library-sha256 must be 64 lowercase hex digits"
                );
                let shown = library.display();
                ensure!(
                    !shown.to_string().contains(['{', '}']),
                    "{target}: library {shown} has a placeholder other than {{version}} and \
                     {{platform}}"
                );
                ensure!(
                    library.file_name().is_some()
                        && library
                            .components()
                            .all(|component| matches!(component, Component::Normal(_))),
                    "{target}: library {shown:?} must be a relative path inside the archive"
                );
                Ok(())
            }
            _ => bail!("{target}: library and library-sha256 go together"),
        }
    }

    /// The path inside the archive of the library of `asset`, if it has one.
    fn library_member(&self, asset: &Asset) -> Option<PathBuf> {
        asset
            .library
            .as_ref()
            .map(|template| PathBuf::from(self.render(template, asset)))
    }

    fn source(&self) -> anyhow::Result<Source<'_>> {
        match (&self.url, &self.crate_name, &self.sha256) {
            (Some(url), None, None) if self.features.is_none() => Ok(Source::Release { url }),
            (None, Some(name), Some(sha256))
                if self.targets.is_empty()
                    && self.binary.is_none()
                    && self.checksums.is_none()
                    && self.attestations.is_none()
                    && self.release_digests.is_none() =>
            {
                Ok(Source::Crate { name, sha256 })
            }
            _ => bail!(
                "set either `url` with `targets` for release assets, or `crate` with `sha256` \
                 (and optionally `features`) for a crate built from crates.io"
            ),
        }
    }

    /// Where to download the tool for `target`: its release asset or its `.crate` file.
    fn download_url(&self, target: Target) -> anyhow::Result<String> {
        Ok(match self.source()? {
            Source::Release { url } => self.render(url, self.asset(target)?),
            Source::Crate { name, .. } => format!(
                "https://static.crates.io/crates/{name}/{name}-{}.crate",
                self.version
            ),
        })
    }

    /// The sha256 the installed executable must have: pinned for a release, recorded at
    /// install for a crate, whose build is local. `None` when nothing was recorded.
    fn executable_pin(&self, root: &Path, target: Target) -> anyhow::Result<Option<String>> {
        match self.source()? {
            Source::Release { .. } => Ok(Some(self.asset(target)?.binary_sha256.clone())),
            Source::Crate { .. } => {
                let record = self.recorded_sha256(root);
                Ok(fs::read_to_string(&record)
                    .ok()
                    .map(|text| text.trim().to_owned()))
            }
        }
    }

    fn recorded_sha256(&self, root: &Path) -> PathBuf {
        let mut record = self.version_dir(root).join(&self.name).into_os_string();
        record.push(".sha256");
        PathBuf::from(record)
    }

    fn asset(&self, target: Target) -> anyhow::Result<&Asset> {
        self.targets
            .get(&target)
            .with_context(|| format!("no asset for {target}"))
    }

    /// The executable in a downloaded asset: a member of the archive, or the asset itself.
    fn executable_in(&self, download: &[u8], asset: &Asset) -> anyhow::Result<Vec<u8>> {
        match &self.binary {
            Some(member) => extract(download, Path::new(&self.render(member, asset))),
            None => Ok(download.to_vec()),
        }
    }

    fn render(&self, template: &str, asset: &Asset) -> String {
        template
            .replace("{version}", &self.version)
            .replace("{platform}", &asset.platform)
    }

    /// The pinned library for `target`, where it is installed and its sha256, if the tool has one.
    fn library(&self, root: &Path, target: Target) -> anyhow::Result<Option<(PathBuf, &str)>> {
        let asset = self.asset(target)?;
        Ok(self
            .library_member(asset)
            .zip(asset.library_sha256.as_deref())
            .and_then(|(member, sha256)| {
                let name = member.file_name()?.to_os_string();
                Some((self.version_dir(root).join(name), sha256))
            }))
    }

    /// The directory the tool's version is installed in.
    fn version_dir(&self, root: &Path) -> PathBuf {
        root.join(TOOLS_DIR).join(&self.name).join(&self.version)
    }

    /// The installed executable for `target`: named after the tool, or at its path in the
    /// installed directory of a tool installed as one.
    fn executable(&self, root: &Path, target: Target) -> anyhow::Result<PathBuf> {
        let dir = self.version_dir(root);
        match (&self.binary, self.tree) {
            (Some(member), true) => Ok(dir.join(self.render(member, self.asset(target)?))),
            _ => Ok(dir.join(&self.name)),
        }
    }

    /// The pinned sha256 of the tool's directory for `target`, if it is installed as one.
    fn tree_pin(&self, target: Target) -> anyhow::Result<Option<&str>> {
        if !self.tree {
            return Ok(None);
        }
        Ok(self.asset(target)?.tree_sha256.as_deref())
    }

    /// The installed executable, once its sha256 matches the pin; otherwise an error saying how
    /// to install it.
    pub(crate) fn installed(&self, root: &Path) -> anyhow::Result<PathBuf> {
        let target = Target::host()?;
        let executable = self.executable(root, target)?;
        ensure!(
            executable.is_file(),
            "{} {} is not installed: run `cargo xtask setup`",
            self.name,
            self.version
        );
        if let Some(pinned) = self.tree_pin(target)? {
            let dir = self.version_dir(root);
            let actual = Tree::of_dir(&dir)?.sha256();
            ensure!(
                actual == pinned,
                "{} holds files with sha256 {actual}, but its pin is {pinned}: run `cargo xtask \
                 setup`",
                dir.display()
            );
            return Ok(executable);
        }
        let pinned = self.executable_pin(root, target)?.with_context(|| {
            format!(
                "{} has no recorded sha256: run `cargo xtask setup`",
                self.name
            )
        })?;
        let actual = file_sha256(&executable)?;
        ensure!(
            actual == pinned,
            "{} has sha256 {actual}, but its pin is {pinned}: run `cargo xtask setup`",
            executable.display()
        );
        if let Source::Release { .. } = self.source()?
            && let Some((library, library_pin)) = self.library(root, target)?
        {
            let library_sha256 = file_sha256(&library).with_context(|| {
                format!(
                    "{} is not installed: run `cargo xtask setup`",
                    library.display()
                )
            })?;
            ensure!(
                library_sha256 == library_pin,
                "{} has sha256 {library_sha256}, but its pin is {library_pin}: run `cargo xtask \
                 setup`",
                library.display()
            );
        }
        Ok(executable)
    }

    fn status(&self, root: &Path, target: Target) -> anyhow::Result<Status> {
        let executable = self.executable(root, target)?;
        if !executable.is_file() {
            return Ok(Status::Missing);
        }
        if let Some(pinned) = self.tree_pin(target)? {
            let sha256 = Tree::of_dir(&self.version_dir(root))?.sha256();
            if sha256 != pinned {
                return Ok(Status::TreeModified { sha256 });
            }
        } else {
            let sha256 = file_sha256(&executable)?;
            if self.executable_pin(root, target)?.as_deref() != Some(sha256.as_str()) {
                return Ok(Status::Modified { sha256 });
            }
        }
        if let Source::Release { .. } = self.source()?
            && let Some((library, library_pin)) = self.library(root, target)?
        {
            if !library.is_file() {
                return Ok(Status::Missing);
            }
            let library_sha256 = file_sha256(&library)?;
            if library_sha256 != library_pin {
                return Ok(Status::LibraryModified {
                    sha256: library_sha256,
                });
            }
        }
        let version_args = self
            .version_args
            .clone()
            .unwrap_or_else(|| vec!["--version".to_owned()]);
        let output = Command::new(&executable)
            .args(version_args)
            .output()
            .with_context(|| format!("could not run {} --version", executable.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() && reports_version(&format!("{stdout}\n{stderr}"), &self.version)
        {
            Ok(Status::Ready)
        } else {
            Ok(Status::Drift {
                said: version_run(output.status, &stdout, &stderr),
            })
        }
    }
}

/// What a version run that did not report the pinned version did, as the rest of a sentence
/// beginning "`--version` …": the version it reported, or, when it failed or printed nothing, its
/// exit status and what it said on stderr, so a binary that cannot even start says why.
fn version_run(status: ExitStatus, stdout: &str, stderr: &str) -> String {
    let reported = stdout.lines().map(str::trim).find(|line| !line.is_empty());
    if status.success()
        && let Some(reported) = reported
    {
        return format!("reports {reported:?}");
    }
    let stderr = stderr.trim();
    let said = if stderr.is_empty() {
        "nothing on stderr".to_owned()
    } else {
        format!("on stderr: {stderr}")
    };
    match reported {
        Some(reported) => format!("{status}, reporting {reported:?}, with {said}"),
        None => format!("{status} and prints nothing on stdout, with {said}"),
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether `--version` output names `version` as a whole word, with or without a leading `v`.
fn reports_version(output: &str, version: &str) -> bool {
    output
        .split_whitespace()
        .any(|word| word.strip_prefix('v').unwrap_or(word) == version)
}

/// An installed tool's state.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Ready,
    Missing,
    /// Installed, but the executable is not the pinned one.
    Modified {
        sha256: String,
    },
    /// Installed, but its library is not the pinned one.
    LibraryModified {
        sha256: String,
    },
    /// Installed as a directory, but the directory's files are not the pinned ones.
    TreeModified {
        sha256: String,
    },
    /// Installed with the pinned sha256, but `--version` does not report the pinned version:
    /// `said` is what it did instead, as [`version_run`] words it.
    Drift {
        said: String,
    },
}

/// `cargo xtask setup`: installs every pinned tool, or the `only` ones and with `for_tests` those
/// the tests run, that is missing or drifted.
pub(crate) fn setup(
    root: &Path,
    only: &[String],
    for_tests: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let manifest = Manifest::load(root)?.select(only, for_tests)?;
    install_unready(root, &manifest, Target::host()?, out, &mut download)
}

/// Installs, with archives from `fetch`, every tool that is not ready: missing, modified, or
/// reporting another version.
fn install_unready(
    root: &Path,
    manifest: &Manifest,
    target: Target,
    out: &mut impl Write,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<()> {
    for tool in &manifest.tools {
        if tool.status(root, target)? == Status::Ready {
            writeln!(out, "ok       {} {}", tool.name, tool.version)?;
        } else {
            let url = tool.download_url(target)?;
            writeln!(out, "install  {} {} from {url}", tool.name, tool.version)?;
            install(root, tool, target, &fetch(&url)?)?;
        }
        remove_other_versions(root, tool)?;
    }
    Ok(())
}

/// `cargo xtask doctor`: reports the state of each tool, or the `only` ones and with `for_tests`
/// those the tests run, and fails unless all are ready.
pub(crate) fn doctor(
    root: &Path,
    only: &[String],
    for_tests: bool,
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let manifest = Manifest::load(root)?.select(only, for_tests)?;
    report(root, &manifest, Target::host()?, out)
}

fn report(
    root: &Path,
    manifest: &Manifest,
    target: Target,
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let mut ready = true;
    for tool in &manifest.tools {
        match tool.status(root, target)? {
            Status::Ready => writeln!(out, "ok       {} {}", tool.name, tool.version)?,
            Status::Missing => {
                ready = false;
                writeln!(out, "missing  {} {}", tool.name, tool.version)?;
            }
            Status::Modified { sha256 } => {
                ready = false;
                writeln!(
                    out,
                    "modified {} {}: the installed binary has sha256 {sha256}, its pin is {}",
                    tool.name,
                    tool.version,
                    tool.executable_pin(root, target)?
                        .unwrap_or_else(|| "missing".to_owned())
                )?;
            }
            Status::LibraryModified { sha256 } => {
                ready = false;
                writeln!(
                    out,
                    "modified {} {}: the installed library has sha256 {sha256}, not its pin",
                    tool.name, tool.version
                )?;
            }
            Status::TreeModified { sha256 } => {
                ready = false;
                writeln!(
                    out,
                    "modified {} {}: the installed directory's files have sha256 {sha256}, not \
                     its pin",
                    tool.name, tool.version
                )?;
            }
            Status::Drift { said } => {
                ready = false;
                writeln!(
                    out,
                    "drift    {} {}: the installed binary's `--version` {said}",
                    tool.name, tool.version
                )?;
            }
        }
    }
    if ready {
        Ok(ExitCode::SUCCESS)
    } else {
        writeln!(
            out,
            "run `cargo xtask setup` to install the pinned versions"
        )?;
        Ok(ExitCode::FAILURE)
    }
}

/// `cargo xtask pin`: downloads every tool for every target and records the sha256 of each
/// archive and of the executable in it, keeping the tool list's layout and comments.
pub(crate) fn pin(root: &Path, out: &mut impl Write) -> anyhow::Result<()> {
    let path = root.join(MANIFEST);
    let text =
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
    let pinned = pin_text(&text, out, &mut download)?;
    fs::write(&path, pinned).with_context(|| format!("could not write {}", path.display()))
}

/// The tool list `text` with every sha256 recomputed from downloads by `fetch`. Where a tool
/// publishes checksums, each download must match them.
fn pin_text(
    text: &str,
    out: &mut impl Write,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<String> {
    let manifest = Manifest::parse(text).with_context(|| format!("{MANIFEST} is invalid"))?;
    let mut document: DocumentMut = text.parse()?;
    let mut published: BTreeMap<String, String> = BTreeMap::new();
    for tool in &manifest.tools {
        if let Source::Crate {
            name,
            sha256: pinned,
        } = tool.source()?
        {
            let sha256 = sha256_hex(&fetch(&tool.download_url(Target::MacArm64)?)?);
            check_indexed(name, &tool.version, &sha256, fetch)
                .with_context(|| format!("{} {}", tool.name, tool.version))?;
            let state = if sha256 == pinned { "same" } else { "changed" };
            writeln!(
                out,
                "{state:<8} {} {}: matches the crates.io index",
                tool.name, tool.version
            )?;
            set_crate_pin(&mut document, &tool.name, &sha256)?;
            continue;
        }
        for target in Target::ALL {
            let asset = tool.asset(target)?;
            let url = tool.download_url(target)?;
            let archive = fetch(&url)?;
            let sha256 = sha256_hex(&archive);
            let mut sources = Vec::new();
            if let Some(template) = &tool.checksums {
                let checksums_url = tool.render(template, asset);
                check_published(&checksums_url, &url, &sha256, &mut published, fetch)
                    .with_context(|| format!("{} {} on {target}", tool.name, tool.version))?;
                sources.push("matches the published checksum".to_owned());
            }
            if let Some(repository) = &tool.attestations {
                check_attested(repository, &sha256, fetch)
                    .with_context(|| format!("{} {} on {target}", tool.name, tool.version))?;
                sources.push(format!("attested on GitHub by {repository}"));
            }
            if let Some(template) = &tool.release_digests {
                let release_url = tool.render(template, asset);
                check_release_digest(&release_url, &url, &sha256, &mut published, fetch)
                    .with_context(|| format!("{} {} on {target}", tool.name, tool.version))?;
                sources.push("matches the digest GitHub recorded on upload".to_owned());
            }
            let binary_sha256 = sha256_hex(&tool.executable_in(&archive, asset)?);
            let tree_same = !tool.tree
                || asset.tree_sha256.as_deref()
                    == Some(Tree::of_archive(&archive)?.sha256().as_str());
            let state =
                if sha256 == asset.sha256 && binary_sha256 == asset.binary_sha256 && tree_same {
                    "same"
                } else {
                    "changed"
                };
            writeln!(
                out,
                "{state:<8} {} {} {target}: {}",
                tool.name,
                tool.version,
                sources.join("; ")
            )?;
            set_pin(&mut document, &tool.name, target, "sha256", &sha256)?;
            set_pin(
                &mut document,
                &tool.name,
                target,
                "binary-sha256",
                &binary_sha256,
            )?;
            if tool.tree {
                set_pin(
                    &mut document,
                    &tool.name,
                    target,
                    "tree-sha256",
                    &Tree::of_archive(&archive)?.sha256(),
                )?;
            }
            if let Some(member) = tool.library_member(asset) {
                let library_sha256 = sha256_hex(&extract(&archive, &member)?);
                set_pin(
                    &mut document,
                    &tool.name,
                    target,
                    "library-sha256",
                    &library_sha256,
                )?;
            }
        }
    }
    Ok(document.to_string())
}

/// Requires the checksums file at `checksums_url`, fetched once into `published`, to list
/// `sha256` for the asset at `url`.
fn check_published(
    checksums_url: &str,
    url: &str,
    sha256: &str,
    published: &mut BTreeMap<String, String>,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<()> {
    if !published.contains_key(checksums_url) {
        let listing = String::from_utf8(fetch(checksums_url)?)
            .with_context(|| format!("{checksums_url} is not text"))?;
        published.insert(checksums_url.to_owned(), listing);
    }
    let listing = published
        .get(checksums_url)
        .context("the checksums were fetched above")?;
    let file_name = url.rsplit('/').next().unwrap_or(url);
    let expected = published_sha256(listing, file_name)
        .with_context(|| format!("{checksums_url} has no sha256 for {file_name}"))?;
    ensure!(
        expected == sha256,
        "the download's sha256 is {sha256}, but {checksums_url} publishes {expected}"
    );
    Ok(())
}

/// A release as the GitHub API shows it: its assets, each with the digest GitHub recorded.
#[derive(Deserialize)]
struct Release {
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    /// `sha256:<hex>`; absent for assets uploaded before GitHub recorded digests.
    digest: Option<String>,
}

/// Requires the release at `release_url`, fetched once into `published`, to record `sha256` as
/// the digest of the asset at `url`.
fn check_release_digest(
    release_url: &str,
    url: &str,
    sha256: &str,
    published: &mut BTreeMap<String, String>,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<()> {
    if !published.contains_key(release_url) {
        let release = String::from_utf8(fetch(release_url)?)
            .with_context(|| format!("{release_url} is not text"))?;
        published.insert(release_url.to_owned(), release);
    }
    let release: Release = serde_json::from_str(
        published
            .get(release_url)
            .context("the release was fetched above")?,
    )
    .with_context(|| format!("{release_url} did not answer with a release"))?;
    let file_name = url.rsplit('/').next().unwrap_or(url);
    let digest = release
        .assets
        .iter()
        .find(|asset| asset.name == file_name)
        .with_context(|| format!("{release_url} has no asset {file_name}"))?
        .digest
        .as_deref()
        .with_context(|| format!("{release_url} records no digest for {file_name}"))?;
    let expected = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("{release_url} records {digest} for {file_name}, not a sha256"))?;
    ensure!(
        expected == sha256,
        "the download's sha256 is {sha256}, but {release_url} records {expected}"
    );
    Ok(())
}

/// Requires the crates.io index to record `sha256` for version `version` of crate `name`.
fn check_indexed(
    name: &str,
    version: &str,
    sha256: &str,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<()> {
    let url = index_url(name);
    let listing = String::from_utf8(fetch(&url)?).with_context(|| format!("{url} is not text"))?;
    let entry = listing
        .lines()
        .filter_map(|line| serde_json::from_str::<IndexEntry>(line).ok())
        .find(|entry| entry.vers == version)
        .with_context(|| format!("the crates.io index has no {name} {version}"))?;
    ensure!(
        entry.cksum == sha256,
        "the download's sha256 is {sha256}, but the crates.io index records {}",
        entry.cksum
    );
    Ok(())
}

/// The attestations GitHub stores for one digest.
#[derive(Deserialize)]
struct Attestations {
    attestations: Vec<serde_json::Value>,
}

/// Requires GitHub to hold a build attestation from `repository` for `sha256`. Only a workflow in
/// that repository, or GitHub on its release, can create one.
fn check_attested(
    repository: &str,
    sha256: &str,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{repository}/attestations/sha256:{sha256}");
    let found: Attestations = serde_json::from_slice(
        &fetch(&url).with_context(|| format!("{repository} has no attestation for {sha256}"))?,
    )
    .with_context(|| format!("{url} did not answer with attestations"))?;
    ensure!(
        !found.attestations.is_empty(),
        "{repository} has no attestation for {sha256}"
    );
    Ok(())
}

/// The executable of the tool `name` pinned in the repository at `home`, checked against its pin.
pub(crate) fn pinned(home: &Path, name: &str) -> anyhow::Result<PathBuf> {
    Manifest::load(home)?.tool(name)?.installed(home)
}

/// `cargo xtask tool`: runs a pinned tool's `executable` with the given arguments in `root`.
pub(crate) fn run(executable: &Path, root: &Path, args: &[OsString]) -> anyhow::Result<ExitCode> {
    let status = Command::new(executable)
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("could not run {}", executable.display()))?;
    Ok(ExitCode::from(status_code(status)))
}

/// `PATH` with the directory of the installed tool `executable` first, for tools that run another
/// as a cargo subcommand, such as cargo-llvm-cov running cargo-nextest.
pub(crate) fn path_with(executable: &Path) -> anyhow::Result<OsString> {
    let directory = executable
        .parent()
        .with_context(|| format!("{} has no directory", executable.display()))?;
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .context("PATH holds a directory that cannot be joined")
}

/// The exit code that passes a child's status on; 1 when it has none, as after a signal.
pub(crate) fn status_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

/// Writes `input` to `child`'s piped stdin and closes it, saying whether the child took all of it.
/// A child that exits without reading its input, such as a hook that only prints a note or a
/// command that fails at once, closes the pipe first; its exit status then decides the outcome, so
/// a broken pipe is not an error.
pub(crate) fn feed(child: &mut Child, input: &mut dyn Read) -> std::io::Result<bool> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("the child's input is not piped"))?;
    match std::io::copy(input, &mut stdin) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error),
    }
}

/// Downloads `url`, up to [`MAX_ARCHIVE_BYTES`].
pub(crate) fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("could not download {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_ARCHIVE_BYTES)
        .read_to_vec()
        .with_context(|| format!("could not read {url}"))
}

/// The sha256 a published checksums file gives for `file_name`. The file either lists
/// `<sha256> <name>` lines (`*<name>` in binary mode), or holds the one asset's sha256 alone.
fn published_sha256(listing: &str, file_name: &str) -> Option<String> {
    let lines: Vec<Vec<&str>> = listing
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|words| !words.is_empty())
        .collect();
    let listed = lines.iter().find_map(|words| match words.as_slice() {
        [hash, name] if name.strip_prefix('*').unwrap_or(name) == file_name => Some(*hash),
        _ => None,
    });
    let alone = match lines.as_slice() {
        [words] => match words.as_slice() {
            [hash] => Some(*hash),
            _ => None,
        },
        _ => None,
    };
    listed
        .or(alone)
        .map(str::to_ascii_lowercase)
        .filter(|hash| is_sha256_hex(hash))
}

/// The sha256 of `bytes`, in lowercase hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        for nibble in [byte.wrapping_shr(4), byte & 0x0f] {
            hex.extend(char::from_digit(u32::from(nibble), 16));
        }
    }
    hex
}

/// Verifies a downloaded release asset or `.crate` file and installs the tool's executable from
/// it.
fn install(root: &Path, tool: &Tool, target: Target, download: &[u8]) -> anyhow::Result<()> {
    let (binary, library) = match tool.source()? {
        Source::Release { .. } => verified_release(tool, target, download)?,
        Source::Crate { name, sha256 } => {
            let actual = sha256_hex(download);
            ensure!(
                actual == sha256,
                "sha256 mismatch for the {name} {} crate: {MANIFEST} pins {sha256}, the download is \
                 {actual}. If the version changed, run `cargo xtask pin`.",
                tool.version
            );
            (build_crate(root, tool, name, download)?, None)
        }
    };
    if tool.tree {
        return place(root, tool, target, |staging| {
            tar::Archive::new(GzDecoder::new(download))
                .unpack(staging)
                .context("could not unpack the archive")
        });
    }
    place(root, tool, target, |staging| {
        place_files(tool, staging, &binary, library.as_ref())
    })
}

/// A library to install beside an executable: its file name and contents.
type Library = (OsString, Vec<u8>);

/// The executable in a verified release asset, and its library if it has one.
fn verified_release(
    tool: &Tool,
    target: Target,
    archive: &[u8],
) -> anyhow::Result<(Vec<u8>, Option<Library>)> {
    let asset = tool.asset(target)?;
    let actual = sha256_hex(archive);
    ensure!(
        actual == asset.sha256,
        "sha256 mismatch for {} {} on {target}: {MANIFEST} pins {}, the download is {actual}. \
         If the version changed, run `cargo xtask pin` to record the new checksums.",
        tool.name,
        tool.version,
        asset.sha256
    );
    let binary = tool.executable_in(archive, asset)?;
    let binary_sha256 = sha256_hex(&binary);
    ensure!(
        binary_sha256 == asset.binary_sha256,
        "sha256 mismatch for the {} {} executable on {target}: {MANIFEST} pins {}, the archive \
         holds {binary_sha256}",
        tool.name,
        tool.version,
        asset.binary_sha256
    );
    if let Some(pinned) = tool.tree_pin(target)? {
        let sha256 = Tree::of_archive(archive)?.sha256();
        ensure!(
            sha256 == pinned,
            "sha256 mismatch for the {} {} directory on {target}: {MANIFEST} pins {pinned}, the \
             archive's files have {sha256}",
            tool.name,
            tool.version
        );
    }
    let library = match (tool.library_member(asset), &asset.library_sha256) {
        (Some(member), Some(pinned)) => {
            let bytes = extract(archive, &member)?;
            let sha256 = sha256_hex(&bytes);
            ensure!(
                &sha256 == pinned,
                "sha256 mismatch for the {} {} library on {target}: {MANIFEST} pins {pinned}, the \
                 archive holds {sha256}",
                tool.name,
                tool.version
            );
            let name = member.file_name().context("the library has no file name")?;
            Some((name.to_os_string(), bytes))
        }
        _ => None,
    };
    Ok((binary, library))
}

/// Builds the executable of `tool` from the verified `.crate` file of crate `name`, with the
/// crate's own lockfile.
fn build_crate(root: &Path, tool: &Tool, name: &str, archive: &[u8]) -> anyhow::Result<Vec<u8>> {
    let work = root
        .join(TOOLS_DIR)
        .join(&tool.name)
        .join(format!("{}.build", tool.version));
    remove_dir_if_present(&work)?;
    let sources = work.join("src");
    fs::create_dir_all(&sources)
        .with_context(|| format!("could not create {}", sources.display()))?;
    tar::Archive::new(GzDecoder::new(archive))
        .unpack(&sources)
        .context("the crate is not a gzipped tarball")?;
    let mut install = Command::new(crate::check::cargo());
    install.args(["install", "--locked", "--quiet"]);
    if let Some(features) = &tool.features {
        install
            .args(["--no-default-features", "--features"])
            .arg(features.join(","));
    }
    let status = install
        .arg("--path")
        .arg(sources.join(format!("{name}-{}", tool.version)))
        .arg("--root")
        .arg(work.join("root"))
        .arg("--target-dir")
        .arg(work.join("target"))
        .current_dir(&work)
        .status()
        .context("could not run cargo install")?;
    ensure!(
        status.success(),
        "cargo install of {name} {} failed",
        tool.version
    );
    let built = work.join("root/bin").join(&tool.name);
    let binary = fs::read(&built).with_context(|| format!("could not read {}", built.display()))?;
    remove_dir_if_present(&work)?;
    Ok(binary)
}

/// Installs the tool's version from what `fill` writes into an empty staging directory, moved
/// into place whole, and checks it.
fn place(
    root: &Path,
    tool: &Tool,
    target: Target,
    fill: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let tool_dir = root.join(TOOLS_DIR).join(&tool.name);
    let staging = tool_dir.join(format!("{}.partial", tool.version));
    let destination = tool.version_dir(root);
    remove_dir_if_present(&staging)?;
    fs::create_dir_all(&staging)
        .with_context(|| format!("could not create {}", staging.display()))?;
    fill(&staging)?;
    remove_dir_if_present(&destination)?;
    fs::rename(&staging, &destination)
        .with_context(|| format!("could not move {} into place", staging.display()))?;

    match tool.status(root, target)? {
        Status::Ready => Ok(()),
        Status::Missing => bail!("{} vanished after install", tool.name),
        Status::Modified { sha256 }
        | Status::LibraryModified { sha256 }
        | Status::TreeModified { sha256 } => bail!(
            "{} changed while it was installed: a sha256 is now {sha256}",
            tool.name
        ),
        Status::Drift { said } => bail!(
            "{} {} was installed, but its `--version` {said}",
            tool.name,
            tool.version
        ),
    }
}

/// Writes `binary` into `staging` as the tool's executable, with `library` beside it, and for a
/// crate the executable's sha256.
fn place_files(
    tool: &Tool,
    staging: &Path,
    binary: &[u8],
    library: Option<&Library>,
) -> anyhow::Result<()> {
    let executable = staging.join(&tool.name);
    fs::write(&executable, binary)
        .with_context(|| format!("could not write {}", executable.display()))?;
    fs::set_permissions(&executable, Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", executable.display()))?;
    if let Some((name, bytes)) = library {
        let path = staging.join(name);
        fs::write(&path, bytes).with_context(|| format!("could not write {}", path.display()))?;
    }
    if let Source::Crate { .. } = tool.source()? {
        let mut record = executable.into_os_string();
        record.push(".sha256");
        fs::write(PathBuf::from(record), sha256_hex(binary))
            .context("could not record the executable's sha256")?;
    }
    Ok(())
}

/// The files of a tool installed as a directory: each path relative to the directory, with its
/// sha256 and whether it is executable. Directories are implied by the files in them; anything
/// else, such as a symbolic link, is refused, so the manifest covers everything the tool can load.
#[derive(Debug, PartialEq, Eq)]
struct Tree(BTreeMap<String, (String, bool)>);

impl Tree {
    /// The files of a gzipped tarball as it unpacks: a hard link holds what its target holds.
    fn of_archive(archive: &[u8]) -> anyhow::Result<Self> {
        let mut files = BTreeMap::new();
        let mut tarball = tar::Archive::new(GzDecoder::new(archive));
        for entry in tarball
            .entries()
            .context("the archive is not a gzipped tarball")?
        {
            let mut entry = entry.context("the archive is corrupt")?;
            let path = tree_path(&entry.path().context("the archive has an unreadable path")?)?;
            let kind = entry.header().entry_type();
            let mode = entry.header().mode().context("an entry has no mode")?;
            if kind.is_file() {
                let mut bytes = Vec::new();
                entry
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("could not read {path} from the archive"))?;
                files.insert(path, (sha256_hex(&bytes), mode & 0o100 != 0));
            } else if kind.is_hard_link() {
                let target = entry
                    .link_name()
                    .context("a hard link has an unreadable target")?
                    .with_context(|| format!("the hard link {path} has no target"))?;
                let target = tree_path(&target)?;
                let (sha256, _) = files
                    .get(&target)
                    .with_context(|| format!("{path} links to {target}, which comes after it"))?
                    .clone();
                files.insert(path, (sha256, mode & 0o100 != 0));
            } else if !kind.is_dir() {
                bail!("{path} in the archive is neither a file nor a directory");
            }
        }
        Ok(Self(files))
    }

    /// The files under `dir`, as installed.
    fn of_dir(dir: &Path) -> anyhow::Result<Self> {
        let mut files = BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in fs::read_dir(&current)
                .with_context(|| format!("could not list {}", current.display()))?
            {
                let path = entry?.path();
                let metadata = fs::symlink_metadata(&path)
                    .with_context(|| format!("could not inspect {}", path.display()))?;
                if metadata.is_dir() {
                    pending.push(path);
                } else if metadata.is_file() {
                    let relative = path
                        .strip_prefix(dir)
                        .context("a listed file lies outside its directory")?;
                    files.insert(
                        tree_path(relative)?,
                        (
                            file_sha256(&path)?,
                            metadata.permissions().mode() & 0o100 != 0,
                        ),
                    );
                } else {
                    bail!("{} is neither a file nor a directory", path.display());
                }
            }
        }
        Ok(Self(files))
    }

    /// The manifest: a line per file, `<sha256> <x or -> <path>`, in path order.
    fn manifest(&self) -> String {
        let mut manifest = String::new();
        for (path, (sha256, executable)) in &self.0 {
            let mode = if *executable { 'x' } else { '-' };
            // Writing to a String cannot fail.
            if writeln!(manifest, "{sha256} {mode} {path}").is_err() {
                break;
            }
        }
        manifest
    }

    /// The sha256 of the manifest, which `tree-sha256` pins.
    fn sha256(&self) -> String {
        sha256_hex(self.manifest().as_bytes())
    }
}

/// `path` as the manifest names it: relative, `/`-separated, without `.` parts, and UTF-8.
fn tree_path(path: &Path) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(
                part.to_str()
                    .with_context(|| format!("{} is not UTF-8", path.display()))?,
            ),
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("{} leaves the tool's directory", path.display())
            }
        }
    }
    ensure!(!parts.is_empty(), "an archive entry has an empty path");
    Ok(parts.join("/"))
}

/// Reads the regular file at `member` out of a gzipped tarball.
fn extract(archive: &[u8], member: &Path) -> anyhow::Result<Vec<u8>> {
    let mut tarball = tar::Archive::new(GzDecoder::new(archive));
    let entries = tarball
        .entries()
        .context("the archive is not a gzipped tarball")?;
    for entry in entries {
        let mut entry = entry.context("the archive is corrupt")?;
        let path = entry.path().context("the archive has an unreadable path")?;
        if path.strip_prefix(".").unwrap_or(&path) == member {
            ensure!(
                entry.header().entry_type().is_file(),
                "{} in the archive is not a regular file",
                member.display()
            );
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .with_context(|| format!("could not read {} from the archive", member.display()))?;
            return Ok(bytes);
        }
    }
    bail!("the archive has no {}", member.display())
}

/// Removes installed versions of `tool` other than the pinned one.
fn remove_other_versions(root: &Path, tool: &Tool) -> anyhow::Result<()> {
    let tool_dir = root.join(TOOLS_DIR).join(&tool.name);
    for entry in
        fs::read_dir(&tool_dir).with_context(|| format!("could not list {}", tool_dir.display()))?
    {
        let path = entry?.path();
        if path.file_name() != Some(tool.version.as_ref()) {
            fs::remove_dir_all(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn remove_dir_if_present(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| format!("could not remove {}", path.display()))?;
    }
    Ok(())
}

/// Sets a crate tool's `sha256` pin in the tool list document.
fn set_crate_pin(document: &mut DocumentMut, name: &str, sha256: &str) -> anyhow::Result<()> {
    let value = document
        .get_mut("tool")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .context("the tool list has no [[tool]] tables")?
        .iter_mut()
        .find(|tool| tool.get("name").and_then(toml_edit::Item::as_str) == Some(name))
        .and_then(|tool| tool.get_mut("sha256"))
        .and_then(toml_edit::Item::as_value_mut)
        .with_context(|| format!("the tool list has no sha256 for {name}"))?;
    let decor = value.decor().clone();
    *value = sha256.into();
    *value.decor_mut() = decor;
    Ok(())
}

/// Sets one of a tool's pins, `sha256` or `binary-sha256`, for one target in the tool list
/// document.
fn set_pin(
    document: &mut DocumentMut,
    name: &str,
    target: Target,
    key: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    let tool = document
        .get_mut("tool")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .context("the tool list has no [[tool]] tables")?
        .iter_mut()
        .find(|tool| tool.get("name").and_then(toml_edit::Item::as_str) == Some(name))
        .with_context(|| format!("the tool list has no tool named {name}"))?;
    let value = tool
        .get_mut("targets")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|targets| targets.get_mut(target.triple()))
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|asset| asset.get_mut(key))
        .and_then(toml_edit::Item::as_value_mut)
        .with_context(|| format!("{name} has no {key} for {target}"))?;
    let decor = value.decor().clone();
    *value = sha256.into();
    *value.decor_mut() = decor;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::ExitCode;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::{
        Manifest, Status, Target, Tool, Tree, doctor, extract, install, install_unready, pin_text,
        published_sha256, remove_other_versions, report, reports_version, set_pin, setup,
        sha256_hex,
    };

    /// Input that cannot be read.
    struct Unreadable;

    impl std::io::Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the disk is gone"))
        }
    }

    #[test]
    fn feeding_reports_input_that_cannot_be_read() {
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let error = super::feed(&mut child, &mut Unreadable).unwrap_err();
        assert_eq!(error.to_string(), "the disk is gone");
        assert!(child.wait().unwrap().success(), "its stdin was closed");
    }

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SCRIPT: &[u8] = b"#!/bin/sh\necho 'fake 1.2.3'\n";

    fn manifest_text(sha256: &str, binary_sha256: &str) -> String {
        format!(
            r#"# The tools.
[[tool]]
name = "fake"
version = "1.2.3"
url = "https://example.com/fake-{{version}}-{{platform}}.tar.gz"
attestations = "example/fake"
binary = "fake-{{version}}/fake"
targets.aarch64-apple-darwin = {{ platform = "mac", sha256 = "{sha256}", binary-sha256 = "{binary_sha256}" }}
targets.x86_64-unknown-linux-gnu = {{ platform = "linux-x64", sha256 = "{sha256}", binary-sha256 = "{binary_sha256}" }}
targets.aarch64-unknown-linux-gnu = {{ platform = "linux-arm64", sha256 = "{sha256}", binary-sha256 = "{binary_sha256}" }}
"#
        )
    }

    /// The tool list pinning `archive`, whose executable is `binary`.
    fn pinned(archive: &[u8], binary: &[u8]) -> Manifest {
        Manifest::parse(&manifest_text(&sha256_hex(archive), &sha256_hex(binary))).unwrap()
    }

    /// What GitHub answers for a digest that `example/fake` attested.
    const ATTESTED: &[u8] = br#"{"attestations":[{"repository_id":1}]}"#;

    /// A fetch that serves `archive`, and attestations for any digest.
    fn serving(archive: &[u8]) -> impl FnMut(&str) -> anyhow::Result<Vec<u8>> {
        let archive = archive.to_vec();
        move |url: &str| {
            Ok(if url.contains("/attestations/") {
                ATTESTED.to_vec()
            } else {
                archive.clone()
            })
        }
    }

    fn only_tool(manifest: &Manifest) -> &Tool {
        manifest.tool("fake").expect("the test manifest lists fake")
    }

    fn tarball(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for &(path, contents, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(contents.len()).unwrap());
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, path, contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn install_from(root: &Path, manifest: &Manifest, archive: &[u8]) -> u32 {
        let downloads = Cell::new(0_u32);
        let mut fetch = |_: &str| {
            downloads.set(downloads.get().saturating_add(1));
            Ok(archive.to_vec())
        };
        install_unready(
            root,
            manifest,
            Target::MacArm64,
            &mut Vec::new(),
            &mut fetch,
        )
        .unwrap();
        downloads.get()
    }

    #[test]
    fn no_checked_in_pin_is_all_zeros() {
        let manifest = Manifest::load(&crate::testing::checkout()).unwrap();
        let zeros = "0".repeat(64);
        for tool in &manifest.tools {
            let mut pins: Vec<(String, &str)> = Vec::new();
            if let Some(sha256) = &tool.sha256 {
                pins.push((format!("{} crate", tool.name), sha256));
            }
            for (target, asset) in &tool.targets {
                pins.push((format!("{} {target} archive", tool.name), &asset.sha256));
                pins.push((
                    format!("{} {target} executable", tool.name),
                    &asset.binary_sha256,
                ));
                if let Some(library) = &asset.library_sha256 {
                    pins.push((format!("{} {target} library", tool.name), library));
                }
            }
            for (what, pin) in pins {
                assert_ne!(pin, zeros, "{what} is unpinned: run `cargo xtask pin`");
            }
        }
    }

    #[test]
    fn checked_in_tool_list_is_valid() {
        let root = crate::testing::checkout();
        let manifest = Manifest::load(&root).unwrap();
        for name in [
            "cargo-nextest",
            "cargo-deny",
            "cargo-shear",
            "zizmor",
            "pinact",
            "gitleaks",
            "osv-scanner",
            "cargo-llvm-cov",
            "cargo-mutants",
        ] {
            manifest.tool(name).unwrap();
        }
    }

    #[test]
    fn parses_a_valid_manifest() {
        let manifest = Manifest::parse(&manifest_text(SHA_A, SHA_B)).unwrap();
        let tool = only_tool(&manifest);
        let asset = tool.asset(Target::LinuxX64).unwrap();
        assert_eq!(
            tool.download_url(Target::LinuxX64).unwrap(),
            "https://example.com/fake-1.2.3-linux-x64.tar.gz",
            "placeholders are filled in"
        );
        assert_eq!(
            tool.binary
                .as_deref()
                .map(|binary| tool.render(binary, asset)),
            Some("fake-1.2.3/fake".to_owned()),
            "the binary path is a template too"
        );
    }

    #[test]
    fn rejects_invalid_manifests() {
        let valid = manifest_text(SHA_A, SHA_B);
        let cases = [
            (
                valid.replace(SHA_A, &SHA_A.to_uppercase()),
                "sha256 must be",
            ),
            (
                valid.replace(SHA_A, "abc"),
                "sha256 must be 64 lowercase hex",
            ),
            (
                valid.replace(SHA_B, "abc"),
                "binary-sha256 must be 64 lowercase hex",
            ),
            (
                valid.replace("targets.aarch64-unknown-linux-gnu", "# none"),
                "no asset for aarch64-unknown-linux-gnu",
            ),
            (valid.replace("https://", "http://"), "https"),
            (
                valid.replace(
                    "binary =",
                    "checksums = \"http://example.com/sums\"\nbinary =",
                ),
                "the checksums url must use https",
            ),
            (valid.replace("{platform}", "{arch}"), "placeholder"),
            (
                valid.replace("fake-{version}/fake", "../fake"),
                "relative path",
            ),
            (
                valid.replace("version = \"1.2.3\"", "version = \"../1\""),
                "version",
            ),
            (valid.replace("name = \"fake\"", "name = \"a/b\""), "name"),
            (
                valid.replace("fake-{version}/fake", ""),
                "binary must not be empty",
            ),
            (
                valid.replace("binary =", "mirror = \"x\"\nbinary ="),
                "unknown field",
            ),
            (
                format!("{valid}\n{}", valid.replace("# The tools.", "")),
                "listed twice",
            ),
        ];
        for (text, expected) in cases {
            let error = format!("{:#}", Manifest::parse(&text).unwrap_err());
            assert!(
                error.contains(expected),
                "expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn version_must_be_a_whole_word() {
        assert!(
            reports_version("cargo-nextest 0.9.150 (abc 2026-10-26)", "0.9.150"),
            "nextest"
        );
        assert!(
            reports_version("Version: 1.14.0\n", "1.14.0"),
            "cargo-shear"
        );
        assert!(reports_version("pinact version 5.0.2", "5.0.2"), "pinact");
        assert!(
            reports_version("v8.30.1", "8.30.1"),
            "a leading v is allowed"
        );
        assert!(
            !reports_version("tool 0.9.1500", "0.9.150"),
            "a longer version is drift"
        );
        assert!(
            !reports_version("tool 10.9.150", "0.9.150"),
            "a prefix is drift"
        );
        assert!(!reports_version("", "1.0.0"), "no output is drift");
    }

    #[test]
    fn extracts_a_member_from_a_tarball() {
        let archive = tarball(&[
            ("README.md", b"readme", 0o644),
            ("fake-1.2.3/fake", b"binary", 0o755),
            ("./top", b"top", 0o755),
        ]);
        assert_eq!(
            extract(&archive, Path::new("fake-1.2.3/fake")).unwrap(),
            b"binary",
            "a nested member"
        );
        assert_eq!(
            extract(&archive, Path::new("top")).unwrap(),
            b"top",
            "a leading ./ is ignored"
        );
        let missing = extract(&archive, Path::new("other")).unwrap_err();
        assert!(
            missing.to_string().contains("the archive has no other"),
            "{missing:#}"
        );
        let garbage = extract(b"not a tarball", Path::new("fake")).unwrap_err();
        assert!(
            format!("{garbage:#}").contains("corrupt") || format!("{garbage:#}").contains("gzip"),
            "{garbage:#}"
        );
    }

    #[test]
    fn installs_a_verified_archive_and_removes_other_versions() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o644)]);
        let manifest = pinned(&archive, SCRIPT);
        let tool = only_tool(&manifest);
        let stale = root.path().join(".tools/fake/1.0.0");
        fs::create_dir_all(&stale).unwrap();

        install(root.path(), tool, Target::MacArm64, &archive).unwrap();
        remove_other_versions(root.path(), tool).unwrap();

        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Ready,
            "the installed script is the pinned one and reports the pinned version"
        );
        assert_eq!(
            tool.installed(root.path()).unwrap(),
            root.path().join(".tools/fake/1.2.3/fake"),
            "the executable lives under its version"
        );
        assert!(!stale.exists(), "other versions are removed");
    }

    #[test]
    fn installs_an_asset_that_is_the_executable_itself() {
        let root = tempfile::tempdir().unwrap();
        let text = manifest_text(&sha256_hex(SCRIPT), &sha256_hex(SCRIPT))
            .replace("binary = \"fake-{version}/fake\"\n", "")
            .replace(".tar.gz", "");
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(install_from(root.path(), &manifest, SCRIPT), 1, "installed");
        let tool = only_tool(&manifest);
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Ready,
            "the raw executable is installed as downloaded"
        );
        let mut fetch = serving(SCRIPT);
        let pinned = pin_text(
            &manifest_text(SHA_A, SHA_B).replace("binary = \"fake-{version}/fake\"\n", ""),
            &mut Vec::new(),
            &mut fetch,
        )
        .unwrap();
        assert_eq!(
            pinned.matches(&sha256_hex(SCRIPT)).count(),
            6,
            "pin records the same sha256 for the asset and the executable on every target"
        );
    }

    #[test]
    fn setup_reinstalls_a_tool_that_reports_another_version() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let manifest = pinned(&archive, SCRIPT);
        let executable = root.path().join(".tools/fake/1.2.3/fake");

        assert_eq!(
            install_from(root.path(), &manifest, &archive),
            1,
            "installed"
        );
        assert_eq!(
            install_from(root.path(), &manifest, &archive),
            0,
            "a ready tool is not downloaded again"
        );
        write_executable(&executable, b"#!/bin/sh\necho 'fake 9.9.9'\n");
        assert_eq!(
            install_from(root.path(), &manifest, &archive),
            1,
            "a tool at another version is downloaded again"
        );
        assert_eq!(
            fs::read(&executable).unwrap(),
            SCRIPT,
            "and replaced by the pinned build"
        );
    }

    const LIBRARY: &[u8] = b"\x7fELF a library";

    /// The tool list pinning `archive`, whose executable is `binary` and whose library is `lib`.
    fn pinned_with_library(archive: &[u8], binary: &[u8], library: &[u8]) -> Manifest {
        let text = manifest_text(&sha256_hex(archive), &sha256_hex(binary)).replace(
            " }\n",
            &format!(
                ", library = \"fake-{{version}}/libfake.so\", library-sha256 = \"{}\" }}\n",
                sha256_hex(library)
            ),
        );
        Manifest::parse(&text).unwrap()
    }

    #[test]
    fn installs_and_checks_a_library_beside_the_executable() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            ("fake-1.2.3/fake", SCRIPT, 0o755),
            ("fake-1.2.3/libfake.so", LIBRARY, 0o644),
        ]);
        let manifest = pinned_with_library(&archive, SCRIPT, LIBRARY);
        let tool = only_tool(&manifest);
        install(root.path(), tool, Target::MacArm64, &archive).unwrap();
        let library = root.path().join(".tools/fake/1.2.3/libfake.so");
        assert_eq!(
            fs::read(&library).unwrap(),
            LIBRARY,
            "beside the executable"
        );
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Ready
        );
        tool.installed(root.path()).unwrap();

        fs::write(&library, b"swapped").unwrap();
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::LibraryModified {
                sha256: sha256_hex(b"swapped")
            }
        );
        let error = tool.installed(root.path()).unwrap_err().to_string();
        assert!(error.contains("libfake.so has sha256"), "{error}");
        let mut out = Vec::new();
        let code = report(root.path(), &manifest, Target::MacArm64, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            printed.contains("the installed library has sha256"),
            "{printed}"
        );

        fs::remove_file(&library).unwrap();
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Missing
        );
        let missing = tool.installed(root.path()).unwrap_err().to_string();
        assert!(missing.contains("libfake.so is not installed"), "{missing}");
        assert_eq!(
            install_from(root.path(), &manifest, &archive),
            1,
            "setup reinstalls it"
        );
        assert_eq!(fs::read(&library).unwrap(), LIBRARY);
    }

    #[test]
    fn refuses_an_archive_whose_library_is_not_the_pinned_one() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            ("fake-1.2.3/fake", SCRIPT, 0o755),
            ("fake-1.2.3/libfake.so", b"other", 0o644),
        ]);
        let manifest = pinned_with_library(&archive, SCRIPT, LIBRARY);
        let error = install(
            root.path(),
            only_tool(&manifest),
            Target::MacArm64,
            &archive,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("sha256 mismatch for the fake 1.2.3 library"),
            "{error}"
        );
        assert!(
            !root.path().join(".tools/fake/1.2.3").exists(),
            "nothing installed"
        );
    }

    #[test]
    fn a_library_needs_its_pin_and_an_archive() {
        let base = manifest_text(SHA_A, SHA_B);
        let with = |extra: &str| base.replace(" }\n", &format!(", {extra} }}\n"));
        for (extra, problem) in [
            (
                "library = \"lib.so\"",
                "library and library-sha256 go together",
            ),
            (
                &format!("library-sha256 = \"{SHA_A}\""),
                "library and library-sha256 go together",
            ),
            (
                "library = \"lib.so\", library-sha256 = \"XYZ\"",
                "library-sha256 must be 64 lowercase hex digits",
            ),
            (
                &format!("library = \"../lib.so\", library-sha256 = \"{SHA_A}\""),
                "must be a relative path inside the archive",
            ),
            (
                &format!("library = \"\", library-sha256 = \"{SHA_A}\""),
                "must be a relative path inside the archive",
            ),
        ] {
            let error = format!("{:#}", Manifest::parse(&with(extra)).unwrap_err());
            assert!(error.contains(problem), "{extra}: {error}");
        }
        let no_archive = with(&format!(
            "library = \"lib.so\", library-sha256 = \"{SHA_A}\""
        ))
        .replace("binary = \"fake-{version}/fake\"\n", "");
        let error = format!("{:#}", Manifest::parse(&no_archive).unwrap_err());
        assert!(
            error.contains("a library comes only from an archive"),
            "{error}"
        );
    }

    #[test]
    fn a_swapped_binary_is_caught_everywhere() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let manifest = pinned(&archive, SCRIPT);
        let tool = only_tool(&manifest);
        install_from(root.path(), &manifest, &archive);
        // Same --version output, different bytes.
        let swapped = b"#!/bin/sh\necho 'fake 1.2.3'\nexit 0\n";
        write_executable(&root.path().join(".tools/fake/1.2.3/fake"), swapped);

        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Modified {
                sha256: sha256_hex(swapped)
            },
            "the version output is not trusted"
        );
        let error = tool.installed(root.path()).unwrap_err().to_string();
        assert!(error.contains("run `cargo xtask setup`"), "{error}");
        let mut out = Vec::new();
        let code = report(root.path(), &manifest, Target::MacArm64, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(code, ExitCode::FAILURE, "doctor fails: {printed}");
        assert!(printed.contains("modified fake 1.2.3"), "{printed}");
    }

    #[test]
    fn refuses_an_archive_with_the_wrong_checksum() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[("fake-1.2.3/fake", b"x", 0o755)]);
        let manifest = Manifest::parse(&manifest_text(SHA_A, SHA_B)).unwrap();
        let error = install(
            root.path(),
            only_tool(&manifest),
            Target::LinuxX64,
            &archive,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("sha256 mismatch"), "{error}");
        assert!(
            error.contains(&sha256_hex(&archive)),
            "names the actual hash: {error}"
        );
        assert!(
            !root.path().join(".tools/fake").exists(),
            "nothing is installed from a mismatched archive"
        );
    }

    #[test]
    fn refuses_an_archive_holding_another_executable() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let manifest = Manifest::parse(&manifest_text(&sha256_hex(&archive), SHA_B)).unwrap();
        let error = install(
            root.path(),
            only_tool(&manifest),
            Target::MacArm64,
            &archive,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("executable"), "{error}");
        assert!(
            !root.path().join(".tools/fake").exists(),
            "nothing is installed"
        );
    }

    #[test]
    fn reports_missing_and_drifted_tools() {
        let root = tempfile::tempdir().unwrap();
        let drifted: &[u8] = b"#!/bin/sh\necho 'fake 1.2.4'\n";
        let manifest = Manifest::parse(&manifest_text(SHA_A, &sha256_hex(drifted))).unwrap();
        let tool = only_tool(&manifest);
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Missing,
            "not installed"
        );
        let error = tool.installed(root.path()).unwrap_err().to_string();
        assert!(error.contains("run `cargo xtask setup`"), "{error}");

        write_executable(&root.path().join(".tools/fake/1.2.3/fake"), drifted);
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Drift {
                said: "reports \"fake 1.2.4\"".to_owned()
            },
            "a pinned binary reporting another version has drifted"
        );
    }

    #[test]
    fn a_version_run_that_fails_says_how_and_why() {
        let broken: &[u8] =
            b"#!/bin/sh\necho 'fake: /lib64/libc.so.6: version GLIBC_2.34 not found' >&2\nexit 1\n";
        let silent: &[u8] = b"#!/bin/sh\n";
        let failing: &[u8] = b"#!/bin/sh\necho 'fake 1.2.3'\nexit 3\n";
        for (script, said, why) in [
            (
                broken,
                "exit status: 1 and prints nothing on stdout, with on stderr: fake: \
                 /lib64/libc.so.6: version GLIBC_2.34 not found",
                "a binary that cannot start says why",
            ),
            (
                silent,
                "exit status: 0 and prints nothing on stdout, with nothing on stderr",
                "a binary that prints nothing",
            ),
            (
                failing,
                "exit status: 3, reporting \"fake 1.2.3\", with nothing on stderr",
                "a run that fails even with the right version",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let manifest = Manifest::parse(&manifest_text(SHA_A, &sha256_hex(script))).unwrap();
            write_executable(&root.path().join(".tools/fake/1.2.3/fake"), script);
            assert_eq!(
                only_tool(&manifest)
                    .status(root.path(), Target::MacArm64)
                    .unwrap(),
                Status::Drift {
                    said: said.to_owned()
                },
                "{why}"
            );
        }
    }

    #[test]
    fn pin_records_the_archive_and_executable_checksums() {
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let text = manifest_text(SHA_A, SHA_B);
        let mut fetch = serving(&archive);
        let pinned = pin_text(&text, &mut Vec::new(), &mut fetch).unwrap();
        let expected = text
            .replace(SHA_A, &sha256_hex(&archive))
            .replace(SHA_B, &sha256_hex(SCRIPT));
        assert_eq!(pinned, expected, "both pins change, nothing else does");
    }

    #[test]
    fn pin_requires_a_match_with_published_checksums() {
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let text = manifest_text(SHA_A, SHA_B).replace(
            "attestations = \"example/fake\"\n",
            "checksums = \"https://example.com/fake-{version}-sums.txt\"\n",
        );
        let listing = |hash: &str| {
            ["mac", "linux-x64", "linux-arm64"]
                .map(|platform| format!("{hash}  fake-1.2.3-{platform}.tar.gz\n"))
                .concat()
        };
        let serve = |sums: String| {
            let archive = archive.clone();
            move |url: &str| {
                Ok(if url.ends_with("sums.txt") {
                    sums.clone().into_bytes()
                } else {
                    archive.clone()
                })
            }
        };

        let mut out = Vec::new();
        let mut matching = serve(listing(&sha256_hex(&archive)));
        let pinned = pin_text(&text, &mut out, &mut matching).unwrap();
        assert!(
            pinned.contains(&sha256_hex(&archive)),
            "a matching download is pinned"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("matches the published checksum"),
            "and reported as verified"
        );

        let mut tampered = serve(listing(SHA_B));
        let mismatch = pin_text(&text, &mut Vec::new(), &mut tampered).unwrap_err();
        assert!(
            format!("{mismatch:#}").contains("publishes"),
            "{mismatch:#}"
        );

        let mut unlisted = serve(format!("{SHA_B}  other.tar.gz\n"));
        let absent = pin_text(&text, &mut Vec::new(), &mut unlisted).unwrap_err();
        assert!(
            format!("{absent:#}").contains("has no sha256 for fake-1.2.3-mac.tar.gz"),
            "{absent:#}"
        );
    }

    #[test]
    fn pin_requires_a_github_attestation() {
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let text = manifest_text(SHA_A, SHA_B);
        let attested_url = format!(
            "https://api.github.com/repos/example/fake/attestations/sha256:{}",
            sha256_hex(&archive)
        );
        let asked = std::cell::RefCell::new(Vec::new());
        let mut out = Vec::new();
        let mut attested = |url: &str| {
            asked.borrow_mut().push(url.to_owned());
            serving(&archive)(url)
        };
        pin_text(&text, &mut out, &mut attested).unwrap();
        assert!(
            asked.borrow().contains(&attested_url),
            "the archive's digest is looked up: {:?}",
            asked.borrow()
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("attested on GitHub by example/fake"),
            "and reported as verified"
        );

        let mut unattested = |url: &str| {
            Ok(if url.contains("/attestations/") {
                br#"{"attestations":[]}"#.to_vec()
            } else {
                archive.clone()
            })
        };
        let error = pin_text(&text, &mut Vec::new(), &mut unattested).unwrap_err();
        assert!(
            format!("{error:#}").contains("example/fake has no attestation"),
            "{error:#}"
        );
    }

    #[test]
    fn pin_requires_the_digest_github_recorded_for_the_release_asset() {
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let text = manifest_text(SHA_A, SHA_B).replace(
            "attestations = \"example/fake\"\n",
            "release-digests = \"https://api.github.com/repos/example/fake/releases/tags/v{version}\"\n",
        );
        let release_url = "https://api.github.com/repos/example/fake/releases/tags/v1.2.3";
        let release = |digest: &str| {
            serde_json::json!({
                "assets": [
                    { "name": "fake-1.2.3-mac.tar.gz", "digest": digest },
                    { "name": "fake-1.2.3-linux-x64.tar.gz", "digest": digest },
                    { "name": "fake-1.2.3-linux-arm64.tar.gz", "digest": digest },
                    { "name": "fake-1.2.3.zip", "digest": null },
                ]
            })
            .to_string()
            .into_bytes()
        };
        let recorded = format!("sha256:{}", sha256_hex(&archive));
        let asked = std::cell::RefCell::new(Vec::new());
        let mut out = Vec::new();
        let mut fetch = |url: &str| {
            asked.borrow_mut().push(url.to_owned());
            Ok(if url == release_url {
                release(&recorded)
            } else {
                archive.clone()
            })
        };
        pin_text(&text, &mut out, &mut fetch).unwrap();
        assert_eq!(
            asked
                .borrow()
                .iter()
                .filter(|url| *url == release_url)
                .count(),
            1,
            "the release is read once: {:?}",
            asked.borrow()
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("matches the digest GitHub recorded on upload"),
            "and reported as verified"
        );
        for (answer, expected) in [
            (
                release(&format!("sha256:{SHA_A}")),
                format!("records {SHA_A}"),
            ),
            (release("md5:abc"), "not a sha256".to_owned()),
            (
                br#"{"assets":[]}"#.to_vec(),
                "has no asset fake-1.2.3-mac.tar.gz".to_owned(),
            ),
            (
                br#"{"assets":[{"name":"fake-1.2.3-mac.tar.gz","digest":null}]}"#.to_vec(),
                "records no digest".to_owned(),
            ),
            (
                b"not json".to_vec(),
                "did not answer with a release".to_owned(),
            ),
        ] {
            let mut wrong = |url: &str| {
                Ok(if url == release_url {
                    answer.clone()
                } else {
                    archive.clone()
                })
            };
            let error = pin_text(&text, &mut Vec::new(), &mut wrong).unwrap_err();
            assert!(
                format!("{error:#}").contains(&expected),
                "{expected}: {error:#}"
            );
        }
        let elsewhere = text.replace("https://api.github.com/repos/", "https://example.com/");
        let refused = format!("{:#}", Manifest::parse(&elsewhere).unwrap_err());
        assert!(refused.contains("a release in the GitHub API"), "{refused}");
    }

    #[test]
    fn a_platform_may_have_dotted_parts_but_no_empty_one() {
        let dotted = manifest_text(SHA_A, SHA_B).replace("\"linux-x64\"", "\"linux.x86_64\"");
        assert!(
            Manifest::parse(&dotted).is_ok(),
            "shellcheck names its assets so"
        );
        for platform in ["\"..\"", "\"linux..x64\"", "\".linux\"", "\"linux/x64\""] {
            let bad = manifest_text(SHA_A, SHA_B).replace("\"linux-x64\"", platform);
            let error = format!("{:#}", Manifest::parse(&bad).unwrap_err());
            assert!(error.contains("joined by '.'"), "{platform}: {error}");
        }
    }

    #[test]
    fn every_tool_needs_a_way_to_verify_its_pin() {
        let unverifiable =
            manifest_text(SHA_A, SHA_B).replace("attestations = \"example/fake\"\n", "");
        let missing = format!("{:#}", Manifest::parse(&unverifiable).unwrap_err());
        assert!(
            missing.contains("needs `checksums`, `attestations` or `release-digests`"),
            "{missing}"
        );
        let malformed = manifest_text(SHA_A, SHA_B).replace("\"example/fake\"", "\"example\"");
        let invalid = format!("{:#}", Manifest::parse(&malformed).unwrap_err());
        assert!(invalid.contains("owner/repo"), "{invalid}");
    }

    fn crate_manifest(sha256: &str) -> String {
        format!(
            "[[tool]]\nname = \"fake\"\nversion = \"1.2.3\"\ncrate = \"fake\"\nsha256 = \"{sha256}\"\n"
        )
    }

    /// The `.crate` file of a binary crate `fake` 1.2.3 that prints its version.
    fn fake_crate() -> Vec<u8> {
        tarball(&[
            (
                "fake-1.2.3/Cargo.toml",
                b"[package]\nname = \"fake\"\nversion = \"1.2.3\"\nedition = \"2021\"\n",
                0o644,
            ),
            (
                "fake-1.2.3/Cargo.lock",
                b"version = 3\n\n[[package]]\nname = \"fake\"\nversion = \"1.2.3\"\n",
                0o644,
            ),
            (
                "fake-1.2.3/src/main.rs",
                b"fn main() {\n    println!(\"fake 1.2.3\");\n}\n",
                0o644,
            ),
        ])
    }

    #[test]
    fn installs_a_crate_tool_with_only_the_pinned_features() {
        let root = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            (
                "fake-1.2.3/Cargo.toml",
                b"[package]\nname = \"fake\"\nversion = \"1.2.3\"\nedition = \"2021\"\n\n[features]\ndefault = [\"wrong\"]\nwrong = []\nright = []\n",
                0o644,
            ),
            (
                "fake-1.2.3/Cargo.lock",
                b"version = 3\n\n[[package]]\nname = \"fake\"\nversion = \"1.2.3\"\n",
                0o644,
            ),
            (
                "fake-1.2.3/src/main.rs",
                b"fn main() {\n    if cfg!(feature = \"right\") && !cfg!(feature = \"wrong\") {\n        println!(\"fake 1.2.3\");\n    }\n}\n",
                0o644,
            ),
        ]);
        let text = format!(
            "{}features = [\"right\"]\n",
            crate_manifest(&sha256_hex(&archive))
        );
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(install_from(root.path(), &manifest, &archive), 1, "built");
        assert_eq!(
            only_tool(&manifest)
                .status(root.path(), Target::MacArm64)
                .unwrap(),
            Status::Ready,
            "built with the pinned feature and without the default one"
        );
    }

    #[test]
    fn crate_tools_are_validated() {
        Manifest::parse(&crate_manifest(SHA_A)).unwrap();
        let mixed = crate_manifest(SHA_A).replace("crate =", "url = \"https://x\"\ncrate =");
        assert!(
            format!("{:#}", Manifest::parse(&mixed).unwrap_err()).contains("set either `url`"),
            "a tool is a release or a crate, not both"
        );
        assert!(
            format!("{:#}", Manifest::parse(&crate_manifest("abc")).unwrap_err())
                .contains("sha256 must be"),
            "the crate's sha256 is checked"
        );
        let release = manifest_text(SHA_A, SHA_B).replace(
            "[[tool]]\nname = \"fake\"\n",
            "[[tool]]\nname = \"fake\"\nfeatures = [\"x\"]\n",
        );
        assert!(
            format!("{:#}", Manifest::parse(&release).unwrap_err()).contains("set either `url`"),
            "features are for crates only"
        );
        for release_only in [
            "binary = \"fake\"",
            "checksums = \"https://x/{version}.sha256\"",
            "attestations = \"o/r\"",
            &format!(
                "targets.aarch64-apple-darwin = {{ platform = \"p\", sha256 = \"{SHA_A}\", binary-sha256 = \"{SHA_B}\" }}"
            ),
        ] {
            let text = format!("{}{release_only}\n", crate_manifest(SHA_A));
            assert!(
                format!("{:#}", Manifest::parse(&text).unwrap_err()).contains("set either `url`"),
                "a crate tool takes no {release_only}"
            );
        }
    }

    #[test]
    fn index_urls_follow_the_crates_io_layout() {
        assert_eq!(
            super::index_url("a"),
            "https://index.crates.io/1/a",
            "one letter"
        );
        assert_eq!(
            super::index_url("ab"),
            "https://index.crates.io/2/ab",
            "two"
        );
        assert_eq!(
            super::index_url("abc"),
            "https://index.crates.io/3/a/abc",
            "three"
        );
        assert_eq!(
            super::index_url("Cargo-Mutants"),
            "https://index.crates.io/ca/rg/cargo-mutants",
            "four or more, lowercased"
        );
    }

    #[test]
    fn pin_checks_a_crate_against_the_crates_io_index() {
        let archive = fake_crate();
        let sha256 = sha256_hex(&archive);
        let serve = |cksum: String| {
            let archive = archive.clone();
            move |url: &str| {
                Ok(if url.starts_with("https://index.crates.io/") {
                    format!("{{\"vers\":\"1.0.0\",\"cksum\":\"{SHA_B}\"}}\n{{\"vers\":\"1.2.3\",\"cksum\":\"{cksum}\"}}\n")
                        .into_bytes()
                } else {
                    assert_eq!(
                        url, "https://static.crates.io/crates/fake/fake-1.2.3.crate",
                        "the crate is downloaded from crates.io"
                    );
                    archive.clone()
                })
            }
        };
        let mut same = Vec::new();
        pin_text(
            &crate_manifest(&sha256),
            &mut same,
            &mut serve(sha256.clone()),
        )
        .unwrap();
        assert!(
            String::from_utf8(same)
                .unwrap()
                .starts_with("same     fake 1.2.3"),
            "an unchanged pin"
        );
        let mut out = Vec::new();
        let pinned =
            pin_text(&crate_manifest(SHA_A), &mut out, &mut serve(sha256.clone())).unwrap();
        assert_eq!(
            pinned,
            crate_manifest(&sha256),
            "the crate's sha256 is pinned"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with("changed  fake 1.2.3: matches the crates.io index"),
            "and reported as changed and verified"
        );
        let error = pin_text(
            &crate_manifest(SHA_A),
            &mut Vec::new(),
            &mut serve(SHA_B.to_owned()),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("the crates.io index records"),
            "{error:#}"
        );
    }

    #[test]
    fn installs_a_crate_tool_by_building_it() {
        let root = tempfile::tempdir().unwrap();
        let archive = fake_crate();
        let manifest = Manifest::parse(&crate_manifest(&sha256_hex(&archive))).unwrap();
        assert_eq!(install_from(root.path(), &manifest, &archive), 1, "built");
        let tool = only_tool(&manifest);
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Ready,
            "the built executable reports its version and matches its recorded sha256"
        );
        assert!(
            !root.path().join(".tools/fake/1.2.3.build").exists(),
            "the build directory is removed"
        );
        write_executable(
            &root.path().join(".tools/fake/1.2.3/fake"),
            b"#!/bin/sh\necho 'fake 1.2.3'\n",
        );
        assert!(
            matches!(
                tool.status(root.path(), Target::MacArm64).unwrap(),
                Status::Modified { .. }
            ),
            "a swapped build is caught by its recorded sha256"
        );
        let tampered = Manifest::parse(&crate_manifest(SHA_A)).unwrap();
        let error = install(
            root.path(),
            only_tool(&tampered),
            Target::MacArm64,
            &archive,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("sha256 mismatch for the fake 1.2.3 crate"),
            "{error}"
        );
    }

    #[test]
    fn setup_and_doctor_can_select_tools() {
        let root = crate::testing::checkout();
        let unknown = ["no-such-tool".to_owned()];
        assert!(
            setup(&root, &unknown, false, &mut Vec::new()).is_err(),
            "setup refuses a tool that is not pinned"
        );
        assert!(
            doctor(&root, &unknown, true, &mut Vec::new()).is_err(),
            "so does doctor"
        );
        let manifest = Manifest::parse(&manifest_text(SHA_A, SHA_B)).unwrap();
        let only = vec!["fake".to_owned()];
        assert_eq!(
            manifest.select(&only, false).unwrap().tools.len(),
            1,
            "selected"
        );
        let error = Manifest::parse(&manifest_text(SHA_A, SHA_B))
            .unwrap()
            .select(&["other".to_owned()], false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("other is not a pinned tool"), "{error}");
    }

    #[test]
    fn for_tests_selects_exactly_the_tools_the_tests_run() {
        let names = |only: &[&str], for_tests: bool| -> Vec<String> {
            let only: Vec<String> = only.iter().map(|&name| name.to_owned()).collect();
            Manifest::load(&crate::testing::checkout())
                .unwrap()
                .select(&only, for_tests)
                .unwrap()
                .tools
                .into_iter()
                .map(|tool| tool.name)
                .collect()
        };
        // #25's Coverage and Mutants jobs installed only cargo-nextest, and xtask's tests failed
        // there for want of shellcheck. node and pnpm build the web app the abc binary embeds,
        // and run its tests.
        assert_eq!(
            names(&[], true),
            ["cargo-nextest", "shellcheck", "node", "pnpm"]
        );
        assert_eq!(
            names(&["cargo-sqlx"], true),
            ["cargo-nextest", "cargo-sqlx", "shellcheck", "node", "pnpm"],
            "named tools join them"
        );
        assert!(names(&[], false).len() > 3, "all of them, with neither");
    }

    #[test]
    fn reads_published_checksum_formats() {
        let listing = format!("{SHA_A}  a.tar.gz\n{SHA_B} *b.tar.gz\n");
        assert_eq!(
            published_sha256(&listing, "a.tar.gz").as_deref(),
            Some(SHA_A),
            "text mode"
        );
        assert_eq!(
            published_sha256(&listing, "b.tar.gz").as_deref(),
            Some(SHA_B),
            "binary mode"
        );
        assert_eq!(published_sha256(&listing, "c.tar.gz"), None, "not listed");
        assert_eq!(
            published_sha256(&format!("{}\n", SHA_A.to_uppercase()), "any.tar.gz").as_deref(),
            Some(SHA_A),
            "a file holding one asset's sha256 alone"
        );
        assert_eq!(
            published_sha256("not-a-hash  a.tar.gz\n", "a.tar.gz"),
            None,
            "not a sha256"
        );
    }

    #[test]
    fn pinning_rewrites_only_the_checksum() {
        let text = manifest_text(SHA_A, SHA_B);
        let mut document: toml_edit::DocumentMut = text.parse().unwrap();
        let new = "c".repeat(64);
        set_pin(&mut document, "fake", Target::LinuxArm64, "sha256", &new).unwrap();
        let expected = text.replace(
            &format!(r#"platform = "linux-arm64", sha256 = "{SHA_A}""#),
            &format!(r#"platform = "linux-arm64", sha256 = "{new}""#),
        );
        assert_eq!(
            document.to_string(),
            expected,
            "comments, layout and other targets are untouched"
        );
        assert!(
            set_pin(&mut document, "other", Target::LinuxArm64, "sha256", &new)
                .is_err_and(|error| error.to_string().contains("no tool named other")),
            "an unknown tool is an error"
        );
    }

    const DIST: &[u8] = b"export const pnpm = true;\n";

    /// The tool list pinning `archive` as a tool installed whole, whose executable is `binary`.
    fn pinned_tree(archive: &[u8], binary: &[u8], tree_sha256: &str) -> Manifest {
        let text = manifest_text(&sha256_hex(archive), &sha256_hex(binary))
            .replace(
                "binary = \"fake-{version}/fake\"\n",
                "binary = \"fake-{version}/fake\"\ntree = true\n",
            )
            .replace(" }\n", &format!(", tree-sha256 = \"{tree_sha256}\" }}\n"));
        Manifest::parse(&text).unwrap()
    }

    fn tree_archive() -> Vec<u8> {
        tarball(&[
            ("fake-1.2.3/fake", SCRIPT, 0o755),
            ("fake-1.2.3/dist/pnpm.mjs", DIST, 0o644),
        ])
    }

    #[test]
    fn installs_a_whole_directory_and_checks_every_file() {
        /// A change to the installed directory, and what it is.
        type Change<'a> = (Box<dyn Fn() + 'a>, &'static str);
        let root = tempfile::tempdir().unwrap();
        let archive = tree_archive();
        let tree = Tree::of_archive(&archive).unwrap();
        assert_eq!(
            tree.manifest(),
            format!(
                "{} - fake-1.2.3/dist/pnpm.mjs\n{} x fake-1.2.3/fake\n",
                sha256_hex(DIST),
                sha256_hex(SCRIPT)
            ),
            "each file with its sha256 and whether it is executable, in path order"
        );
        let manifest = pinned_tree(&archive, SCRIPT, &tree.sha256());
        let tool = only_tool(&manifest);
        install(root.path(), tool, Target::MacArm64, &archive).unwrap();
        let dir = root.path().join(".tools/fake/1.2.3");
        assert_eq!(Tree::of_dir(&dir).unwrap(), tree, "installed as it unpacks");
        assert_eq!(
            tool.status(root.path(), Target::MacArm64).unwrap(),
            Status::Ready
        );
        assert_eq!(
            tool.installed(root.path()).unwrap(),
            dir.join("fake-1.2.3/fake"),
            "the executable is at its path in the directory"
        );

        let dist = dir.join("fake-1.2.3/dist/pnpm.mjs");
        let changes: [Change<'_>; 4] = [
            (
                Box::new(|| fs::write(&dist, b"swapped").unwrap()),
                "a changed file",
            ),
            (
                Box::new(|| fs::write(dir.join("fake-1.2.3/dist/extra.mjs"), b"more").unwrap()),
                "an added file",
            ),
            (
                Box::new(|| fs::remove_file(&dist).unwrap()),
                "a removed file",
            ),
            (
                Box::new(|| fs::set_permissions(&dist, fs::Permissions::from_mode(0o755)).unwrap()),
                "a file made executable",
            ),
        ];
        for (change, what) in changes {
            change();
            let sha256 = Tree::of_dir(&dir).unwrap().sha256();
            assert_eq!(
                tool.status(root.path(), Target::MacArm64).unwrap(),
                Status::TreeModified { sha256 },
                "{what}"
            );
            let error = tool.installed(root.path()).unwrap_err().to_string();
            assert!(error.contains("run `cargo xtask setup`"), "{what}: {error}");
            let mut out = Vec::new();
            let code = report(root.path(), &manifest, Target::MacArm64, &mut out).unwrap();
            let printed = String::from_utf8(out).unwrap();
            assert_eq!(code, ExitCode::FAILURE, "{what}: {printed}");
            assert!(
                printed.contains("modified fake 1.2.3: the installed directory's files"),
                "{what}: {printed}"
            );
            assert_eq!(
                install_from(root.path(), &manifest, &archive),
                1,
                "{what}: setup reinstalls it"
            );
            assert_eq!(Tree::of_dir(&dir).unwrap(), tree, "{what}: restored");
        }
    }

    #[test]
    fn refuses_an_archive_whose_directory_is_not_the_pinned_one() {
        let root = tempfile::tempdir().unwrap();
        let archive = tree_archive();
        let manifest = pinned_tree(&archive, SCRIPT, SHA_A);
        let error = install(
            root.path(),
            only_tool(&manifest),
            Target::MacArm64,
            &archive,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("sha256 mismatch for the fake 1.2.3 directory"),
            "{error}"
        );
        assert!(
            !root.path().join(".tools/fake/1.2.3").exists(),
            "nothing installed"
        );
    }

    /// A gzipped tarball of `fake-1.2.3/fake`, then one more entry of `kind` at `path` that
    /// points at `link`.
    fn tarball_with_link(kind: tar::EntryType, path: &str, link: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(SCRIPT.len()).unwrap());
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "fake-1.2.3/fake", SCRIPT)
            .unwrap();
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(kind);
        link_header.set_size(0);
        link_header.set_mode(0o644);
        builder.append_link(&mut link_header, path, link).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_tree_holds_files_and_hard_links_only() {
        let linked = Tree::of_archive(&tarball_with_link(
            tar::EntryType::Link,
            "fake-1.2.3/pnpm",
            "fake-1.2.3/fake",
        ))
        .unwrap();
        assert_eq!(
            linked.manifest(),
            format!(
                "{0} x fake-1.2.3/fake\n{0} - fake-1.2.3/pnpm\n",
                sha256_hex(SCRIPT)
            ),
            "a hard link holds what its target holds, with a mode of its own"
        );
        for (kind, path, link, problem) in [
            (
                tar::EntryType::Symlink,
                "fake-1.2.3/pnpm",
                "fake",
                "fake-1.2.3/pnpm in the archive is neither a file nor a directory",
            ),
            (
                tar::EntryType::Link,
                "fake-1.2.3/pnpm",
                "fake-1.2.3/later",
                "fake-1.2.3/pnpm links to fake-1.2.3/later, which comes after it",
            ),
            (
                tar::EntryType::Link,
                "fake-1.2.3/pnpm",
                "../outside",
                "../outside leaves the tool's directory",
            ),
        ] {
            let error = format!(
                "{:#}",
                Tree::of_archive(&tarball_with_link(kind, path, link)).unwrap_err()
            );
            assert!(error.contains(problem), "{problem}: {error}");
        }

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), b"x").unwrap();
        std::os::unix::fs::symlink("file", dir.path().join("link")).unwrap();
        let error = format!("{:#}", Tree::of_dir(dir.path()).unwrap_err());
        assert!(
            error.contains("link is neither a file nor a directory"),
            "{error}"
        );
    }

    #[test]
    fn a_tree_needs_its_pin_and_an_archive() {
        let base = manifest_text(SHA_A, SHA_B);
        let tree = |text: &str| {
            text.replace(
                "binary = \"fake-{version}/fake\"\n",
                "binary = \"fake-{version}/fake\"\ntree = true\n",
            )
        };
        let pin = |text: &str, extra: &str| text.replace(" }\n", &format!(", {extra} }}\n"));
        for (text, problem) in [
            (tree(&base), "`tree = true` needs a tree-sha256"),
            (
                pin(&base, &format!("tree-sha256 = \"{SHA_A}\"")),
                "tree-sha256 needs `tree = true`",
            ),
            (
                pin(&tree(&base), "tree-sha256 = \"XYZ\""),
                "tree-sha256 must be 64 lowercase hex digits",
            ),
            (
                pin(
                    &tree(&base),
                    &format!(
                        "tree-sha256 = \"{SHA_A}\", library = \"lib.so\", library-sha256 = \
                         \"{SHA_A}\""
                    ),
                ),
                "a tree holds its libraries itself",
            ),
            (
                pin(&tree(&base), &format!("tree-sha256 = \"{SHA_A}\""))
                    .replace("binary = \"fake-{version}/fake\"\n", ""),
                "a tree comes only from an archive",
            ),
        ] {
            let error = format!("{:#}", Manifest::parse(&text).unwrap_err());
            assert!(error.contains(problem), "{problem}: {error}");
        }
    }

    #[test]
    fn pin_records_a_tree_pin() {
        let archive = tree_archive();
        let text = manifest_text(SHA_A, SHA_B)
            .replace(
                "binary = \"fake-{version}/fake\"\n",
                "binary = \"fake-{version}/fake\"\ntree = true\n",
            )
            .replace(" }\n", &format!(", tree-sha256 = \"{SHA_A}\" }}\n"));
        let mut out = Vec::new();
        let pinned = pin_text(&text, &mut out, &mut serving(&archive)).unwrap();
        let tree = Tree::of_archive(&archive).unwrap().sha256();
        let expected = text
            .replace(
                &format!("tree-sha256 = \"{SHA_A}\""),
                &format!("tree-sha256 = \"{tree}\""),
            )
            .replace(SHA_A, &sha256_hex(&archive))
            .replace(SHA_B, &sha256_hex(SCRIPT));
        assert_eq!(pinned, expected, "the three pins change, nothing else does");
        assert!(String::from_utf8(out).unwrap().contains("changed"));
        let mut again = Vec::new();
        pin_text(&pinned, &mut again, &mut serving(&archive)).unwrap();
        let printed = String::from_utf8(again).unwrap();
        assert!(
            printed.lines().all(|line| line.starts_with("same")),
            "{printed}"
        );
        let stale_tree = pinned.replace(&tree, SHA_B);
        let stale_binary = pinned.replace(&sha256_hex(SCRIPT), SHA_A);
        for (stale, what) in [(stale_tree, "the tree"), (stale_binary, "the executable")] {
            let mut repinned = Vec::new();
            pin_text(&stale, &mut repinned, &mut serving(&archive)).unwrap();
            let states = String::from_utf8(repinned).unwrap();
            assert!(
                states.lines().all(|line| line.starts_with("changed")),
                "a stale pin of {what} alone is a change: {states}"
            );
        }
    }
}
