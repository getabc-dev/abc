//! Embeds the built web app, `web/dist`, in the binary.
//!
//! `cargo xtask build` builds `web/` with the pinned pnpm, then this crate. The script lists every
//! file of `web/dist` into `$OUT_DIR/web.rs` as an `include_bytes!`, so the binary carries the app
//! and the server never reads it from disk. Without a build it fails, naming the command that
//! makes one: a binary that cannot serve its app is never built.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    // crates/abc is two levels below the repository root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2);
    let Some(dist) = root.map(|root| root.join("web/dist")) else {
        println!("cargo::error=crates/abc lies outside a repository");
        return;
    };
    // Cargo scans a directory for changes to anything in it.
    println!("cargo::rerun-if-changed={}", dist.display());
    if let Err(error) = embed(&dist) {
        println!("cargo::error={error}");
    }
}

/// Writes `$OUT_DIR/web.rs`, listing the files under `dist`.
fn embed(dist: &Path) -> Result<(), String> {
    if !dist.join("index.html").is_file() {
        return Err(format!(
            "{} holds no built web app: build abc with `cargo xtask build`, which builds web/ \
             first (`cargo xtask check` does it for you)",
            dist.display()
        ));
    }
    let dist = dist
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", dist.display()))?;
    let mut files = Vec::new();
    list(&dist, &dist, &mut files)?;
    files.sort();
    let mut code = String::from(
        "/// The built web app's files: each path relative to web/dist, with its contents.\n\
         pub(crate) static WEB_FILES: &[(&str, &[u8])] = &[\n",
    );
    for (relative, path) in &files {
        let path = path
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
        writeln!(code, "    ({relative:?}, include_bytes!({path:?})),")
            .map_err(|error| error.to_string())?;
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("cargo sets no OUT_DIR")?);
    fs::write(out.join("web.rs"), code).map_err(|error| format!("cannot write web.rs: {error}"))
}

/// Adds every file under `dir` to `files`, as its path relative to `dist` and its full path.
fn list(dist: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot list {}: {error}", dir.display()))?
            .path();
        let kind = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
            .file_type();
        if kind.is_dir() {
            list(dist, &path, files)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(dist)
                .map_err(|error| error.to_string())?
                .to_str()
                .ok_or_else(|| format!("{} is not UTF-8", path.display()))?
                .replace('\\', "/");
            files.push((relative, path));
        } else {
            return Err(format!(
                "{} is neither a file nor a directory; rebuild with `cargo xtask build`",
                path.display()
            ));
        }
    }
    Ok(())
}
