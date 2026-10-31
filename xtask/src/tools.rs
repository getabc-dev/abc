//! The pinned tools in `xtask/tools.toml`: installing them into `.tools/`, checking them, and
//! running them from there. Nothing here runs a tool from `PATH`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

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

/// One pinned tool.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tool {
    name: String,
    version: String,
    /// Download URL template; `{version}` and `{platform}` are filled in.
    url: String,
    /// Path of the executable inside the archive, with the same placeholders.
    binary: String,
    /// URL template of the checksums the project publishes for its assets, if it publishes any.
    checksums: Option<String>,
    targets: BTreeMap<Target, Asset>,
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
        ensure!(self.url.starts_with("https://"), "the url must use https");
        if let Some(checksums) = &self.checksums {
            ensure!(
                checksums.starts_with("https://"),
                "the checksums url must use https"
            );
        }
        for target in Target::ALL {
            let asset = self.asset(target)?;
            ensure!(
                is_safe_name(&asset.platform),
                "{target}: platform {:?} must be ASCII letters, digits, '-' or '_'",
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
            let url = self.render(&self.url, asset);
            let binary = self.render(&self.binary, asset);
            let checksums = self
                .checksums
                .as_ref()
                .map(|template| self.render(template, asset))
                .unwrap_or_default();
            for (field, value) in [
                ("url", &url),
                ("binary", &binary),
                ("checksums", &checksums),
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
        }
        Ok(())
    }

    fn asset(&self, target: Target) -> anyhow::Result<&Asset> {
        self.targets
            .get(&target)
            .with_context(|| format!("no asset for {target}"))
    }

    /// The executable inside a downloaded archive.
    fn executable_in(&self, archive: &[u8], asset: &Asset) -> anyhow::Result<Vec<u8>> {
        extract(archive, Path::new(&self.render(&self.binary, asset)))
    }

    fn render(&self, template: &str, asset: &Asset) -> String {
        template
            .replace("{version}", &self.version)
            .replace("{platform}", &asset.platform)
    }

    fn executable(&self, root: &Path) -> PathBuf {
        root.join(TOOLS_DIR)
            .join(&self.name)
            .join(&self.version)
            .join(&self.name)
    }

    /// The installed executable, once its sha256 matches the pin; otherwise an error saying how
    /// to install it.
    pub(crate) fn installed(&self, root: &Path) -> anyhow::Result<PathBuf> {
        let executable = self.executable(root);
        ensure!(
            executable.is_file(),
            "{} {} is not installed: run `cargo xtask setup`",
            self.name,
            self.version
        );
        let pinned = &self.asset(Target::host()?)?.binary_sha256;
        let actual = file_sha256(&executable)?;
        ensure!(
            actual == *pinned,
            "{} has sha256 {actual}, but {MANIFEST} pins {pinned}: run `cargo xtask setup`",
            executable.display()
        );
        Ok(executable)
    }

    fn status(&self, root: &Path, target: Target) -> anyhow::Result<Status> {
        let executable = self.executable(root);
        if !executable.is_file() {
            return Ok(Status::Missing);
        }
        let sha256 = file_sha256(&executable)?;
        if sha256 != self.asset(target)?.binary_sha256 {
            return Ok(Status::Modified { sha256 });
        }
        let output = Command::new(&executable)
            .arg("--version")
            .output()
            .with_context(|| format!("could not run {} --version", executable.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() && reports_version(&format!("{stdout}\n{stderr}"), &self.version)
        {
            Ok(Status::Ready)
        } else {
            Ok(Status::Drift {
                reported: stdout.lines().next().unwrap_or_default().trim().to_owned(),
            })
        }
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
    /// Installed with the pinned sha256, but `--version` does not report the pinned version.
    Drift {
        reported: String,
    },
}

/// `cargo xtask setup`: installs every pinned tool that is missing or drifted.
pub(crate) fn setup(root: &Path, out: &mut impl Write) -> anyhow::Result<()> {
    let manifest = Manifest::load(root)?;
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
            let url = tool.render(&tool.url, tool.asset(target)?);
            writeln!(out, "install  {} {} from {url}", tool.name, tool.version)?;
            install(root, tool, target, &fetch(&url)?)?;
        }
        remove_other_versions(root, tool)?;
    }
    Ok(())
}

/// `cargo xtask doctor`: reports each tool's state and fails unless all are ready.
pub(crate) fn doctor(root: &Path, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let manifest = Manifest::load(root)?;
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
                    "modified {} {}: the installed binary has sha256 {sha256}, {MANIFEST} pins {}",
                    tool.name,
                    tool.version,
                    tool.asset(target)?.binary_sha256
                )?;
            }
            Status::Drift { reported } => {
                ready = false;
                writeln!(
                    out,
                    "drift    {} {}: the installed binary reports {reported:?}",
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
        for target in Target::ALL {
            let asset = tool.asset(target)?;
            let url = tool.render(&tool.url, asset);
            let archive = fetch(&url)?;
            let sha256 = sha256_hex(&archive);
            let source = match &tool.checksums {
                Some(template) => {
                    let checksums_url = tool.render(template, asset);
                    if !published.contains_key(&checksums_url) {
                        let listing = String::from_utf8(fetch(&checksums_url)?)
                            .with_context(|| format!("{checksums_url} is not text"))?;
                        published.insert(checksums_url.clone(), listing);
                    }
                    let listing = published
                        .get(&checksums_url)
                        .context("the checksums were fetched above")?;
                    let file_name = url.rsplit('/').next().unwrap_or(&url);
                    let expected = published_sha256(listing, file_name).with_context(|| {
                        format!("{checksums_url} has no sha256 for {file_name}")
                    })?;
                    ensure!(
                        expected == sha256,
                        "{} {} on {target}: the download's sha256 is {sha256}, but {checksums_url} \
                         publishes {expected}",
                        tool.name,
                        tool.version
                    );
                    "matches the published checksum"
                }
                None => "the project publishes no checksums; pinned as downloaded",
            };
            let binary_sha256 = sha256_hex(&tool.executable_in(&archive, asset)?);
            let state = if sha256 == asset.sha256 && binary_sha256 == asset.binary_sha256 {
                "same"
            } else {
                "changed"
            };
            writeln!(
                out,
                "{state:<8} {} {} {target}: {source}",
                tool.name, tool.version
            )?;
            set_pin(&mut document, &tool.name, target, "sha256", &sha256)?;
            set_pin(
                &mut document,
                &tool.name,
                target,
                "binary-sha256",
                &binary_sha256,
            )?;
        }
    }
    Ok(document.to_string())
}

/// `cargo xtask tool`: runs a pinned tool with the given arguments from the repository root.
pub(crate) fn run(root: &Path, name: &str, args: &[OsString]) -> anyhow::Result<ExitCode> {
    let manifest = Manifest::load(root)?;
    let executable = manifest.tool(name)?.installed(root)?;
    let status = Command::new(&executable)
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("could not run {}", executable.display()))?;
    Ok(ExitCode::from(status_code(status)))
}

/// The exit code that passes a child's status on; 1 when it has none, as after a signal.
pub(crate) fn status_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

fn download(url: &str) -> anyhow::Result<Vec<u8>> {
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

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        for nibble in [byte.wrapping_shr(4), byte & 0x0f] {
            hex.extend(char::from_digit(u32::from(nibble), 16));
        }
    }
    hex
}

/// Verifies `archive` and installs the tool's executable from it.
fn install(root: &Path, tool: &Tool, target: Target, archive: &[u8]) -> anyhow::Result<()> {
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

    let tool_dir = root.join(TOOLS_DIR).join(&tool.name);
    let staging = tool_dir.join(format!("{}.partial", tool.version));
    let destination = tool_dir.join(&tool.version);
    remove_dir_if_present(&staging)?;
    fs::create_dir_all(&staging)
        .with_context(|| format!("could not create {}", staging.display()))?;
    let executable = staging.join(&tool.name);
    fs::write(&executable, binary)
        .with_context(|| format!("could not write {}", executable.display()))?;
    fs::set_permissions(&executable, Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", executable.display()))?;
    remove_dir_if_present(&destination)?;
    fs::rename(&staging, &destination)
        .with_context(|| format!("could not move {} into place", staging.display()))?;

    match tool.status(root, target)? {
        Status::Ready => Ok(()),
        Status::Missing => bail!("{} vanished after install", tool.name),
        Status::Modified { sha256 } => bail!(
            "{} changed while it was installed: its sha256 is now {sha256}",
            tool.name
        ),
        Status::Drift { reported } => bail!(
            "{} {} was installed, but `--version` reports {reported:?}",
            tool.name,
            tool.version
        ),
    }
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
        Manifest, Status, Target, Tool, extract, install, install_unready, pin_text,
        published_sha256, remove_other_versions, report, reports_version, set_pin, sha256_hex,
    };

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
    fn checked_in_tool_list_is_valid() {
        let root = crate::repo_root().unwrap();
        let manifest = Manifest::load(&root).unwrap();
        for name in [
            "cargo-nextest",
            "cargo-deny",
            "cargo-shear",
            "zizmor",
            "pinact",
            "gitleaks",
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
            tool.render(&tool.url, asset),
            "https://example.com/fake-1.2.3-linux-x64.tar.gz",
            "placeholders are filled in"
        );
        assert_eq!(
            tool.render(&tool.binary, asset),
            "fake-1.2.3/fake",
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
                reported: "fake 1.2.4".to_owned()
            },
            "a pinned binary reporting another version has drifted"
        );
    }

    #[test]
    fn pin_records_the_archive_and_executable_checksums() {
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let text = manifest_text(SHA_A, SHA_B);
        let mut fetch = |_: &str| Ok(archive.clone());
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
            "binary =",
            "checksums = \"https://example.com/fake-{version}-sums.txt\"\nbinary =",
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
    fn pin_says_when_nothing_is_published() {
        let archive = tarball(&[("fake-1.2.3/fake", SCRIPT, 0o755)]);
        let mut out = Vec::new();
        let mut fetch = |_: &str| Ok(archive.clone());
        pin_text(&manifest_text(SHA_A, SHA_B), &mut out, &mut fetch).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("publishes no checksums"),
            "an unverified pin is never silent"
        );
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
}
