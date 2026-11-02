//! The microsandbox backend: `msb`, pinned in `xtask/tools.toml`, run with an `MSB_HOME` of its
//! own per version, since microsandbox is beta and its releases break compatibility.

use std::ffi::OsString;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

use anyhow::{Context as _, ensure};

use super::{Backend, Spec};

/// microsandbox, as installed by `cargo xtask setup`.
pub(crate) struct Msb {
    /// The `msb` executable; its libkrunfw lies beside it.
    pub(crate) exe: PathBuf,
    /// Its state: images, sandboxes, volumes and their logs.
    pub(crate) home: PathBuf,
}

impl Msb {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.exe);
        command.env("MSB_HOME", &self.home);
        command
    }

    fn run(&self, args: &[OsString]) -> anyhow::Result<Output> {
        let output = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("could not run {}", self.exe.display()))?;
        Ok(output)
    }

    fn succeed(&self, args: &[OsString]) -> anyhow::Result<String> {
        let output = self.run(args)?;
        ensure!(
            output.status.success(),
            "msb {} failed: {}",
            args.first()
                .map(|arg| arg.to_string_lossy())
                .unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// `msb run` arguments that start `spec` detached.
pub(crate) fn run_args(spec: &Spec) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["run", "--detach", "--quiet", "--name"]
        .map(OsString::from)
        .into();
    args.push(spec.name.clone().into());
    args.extend(["--cpus".into(), spec.cpus.to_string().into()]);
    args.extend(["--memory".into(), spec.memory.clone().into()]);
    // Deny by default; DNS, and HTTPS to the allowed hosts, matched by the TLS server name
    // (without intercepting TLS, which would put a CA of microsandbox's in the guest).
    args.extend(["--net-default", "deny", "--net-rule", "allow@dns"].map(OsString::from));
    for host in &spec.egress {
        args.extend(["--net-rule".into(), format!("allow@{host}:tcp:443").into()]);
    }
    args.push("--net-strict=false".into());
    for (host, guest) in &spec.mounts {
        // The agent (uid 1000) owns the files it sees, so it can set their times, as cargo and
        // sqlx do.
        let mut mount = host.clone().into_os_string();
        mount.push(":");
        mount.push(guest);
        mount.push(":uid=1000,gid=1000");
        args.extend(["--mount-dir".into(), mount]);
    }
    for volume in &spec.volumes {
        let mut mount = OsString::from(format!("{}-{}:", spec.name, volume.name));
        mount.push(&volume.guest);
        mount.push(format!(":kind=disk,size={}", volume.size));
        args.extend(["--mount-named".into(), mount]);
    }
    for path in &spec.tmpfs {
        args.extend(["--tmpfs".into(), path.clone().into_os_string()]);
    }
    let mut vsock = spec.vsock.0.clone().into_os_string();
    vsock.push(format!(":{}", spec.vsock.1));
    args.extend(["--vsock".into(), vsock]);
    for (key, value) in &spec.env {
        args.extend(["--env".into(), format!("{key}={value}").into()]);
    }
    args.push(spec.image.clone().into());
    args
}

impl Backend for Msb {
    fn is_running(&self, name: &str) -> anyhow::Result<bool> {
        let listing = self.succeed(&["ls".into(), "--running".into()])?;
        Ok(listing
            .lines()
            .any(|line| line.split_whitespace().next() == Some(name)))
    }

    fn start(&self, spec: &Spec) -> anyhow::Result<()> {
        // A stopped sandbox of the same name holds the old configuration; start afresh.
        self.remove(&spec.name)?;
        self.succeed(&run_args(spec)).map(drop)
    }

    fn exec(&self, name: &str, command: &[OsString], stdin: &[u8]) -> anyhow::Result<Output> {
        let mut child = self
            .command()
            .args(["exec".into(), name.into(), OsString::from("--")])
            .args(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run {}", self.exe.display()))?;
        if let Some(mut input) = child.stdin.take() {
            input
                .write_all(stdin)
                .context("could not write to the command in the VM")?;
        }
        child
            .wait_with_output()
            .context("could not wait for the command in the VM")
    }

    fn attach(&self, name: &str, command: &[OsString]) -> anyhow::Result<ExitStatus> {
        self.command()
            .args([
                "exec".into(),
                "--tty".into(),
                name.into(),
                OsString::from("--"),
            ])
            .args(command)
            .status()
            .with_context(|| format!("could not run {}", self.exe.display()))
    }

    fn remove(&self, name: &str) -> anyhow::Result<()> {
        let output = self.run(&["rm".into(), "--force".into(), name.into()])?;
        let said = String::from_utf8_lossy(&output.stderr);
        ensure!(
            output.status.success() || said.contains("not found"),
            "msb rm failed: {}",
            said.trim()
        );
        Ok(())
    }

    fn remove_volumes(&self, names: &[String]) -> anyhow::Result<()> {
        for name in names {
            let output = self.run(&["volume".into(), "rm".into(), name.into()])?;
            let said = String::from_utf8_lossy(&output.stderr);
            ensure!(
                output.status.success() || said.contains("not found"),
                "msb volume rm {name} failed: {}",
                said.trim()
            );
        }
        Ok(())
    }

    fn list(&self) -> anyhow::Result<String> {
        self.succeed(&["ls".into()])
    }

    fn load_image(&self, archive: &mut dyn Read, tag: &str) -> anyhow::Result<()> {
        let mut child = self
            .command()
            .args(["load", "--quiet", "--tag", tag])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run {}", self.exe.display()))?;
        let mut input = child.stdin.take().context("msb load took no input")?;
        std::io::copy(archive, &mut input).context("could not pass the image to msb load")?;
        drop(input);
        let output = child
            .wait_with_output()
            .context("could not wait for msb load")?;
        ensure!(
            output.status.success(),
            "msb load failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }
}

/// Where microsandbox keeps the state of `version`, under the data directory `data`.
pub(crate) fn home(data: &Path, version: &str) -> PathBuf {
    data.join("abc/msb").join(version)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{Msb, home, run_args};
    use crate::testing::script;
    use crate::vm::{Backend, Spec, Volume};

    fn spec() -> Spec {
        Spec {
            name: "abc-w".to_owned(),
            image: "abc-agent:latest".to_owned(),
            cpus: 6,
            memory: "6G".to_owned(),
            egress: vec![
                "github.com".to_owned(),
                "*.githubusercontent.com".to_owned(),
            ],
            mounts: vec![(PathBuf::from("/w"), PathBuf::from("/w"))],
            volumes: vec![Volume {
                name: "target",
                guest: PathBuf::from("/w/target"),
                size: "40G",
            }],
            tmpfs: vec![PathBuf::from("/run/abc")],
            vsock: (PathBuf::from("/h/b.sock"), 5000),
            env: vec![("ABC_VM".to_owned(), "1".to_owned())],
        }
    }

    #[test]
    fn runs_the_spec_detached_with_the_policy_and_mounts() {
        let args: Vec<String> = run_args(&spec())
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.join(" "),
            "run --detach --quiet --name abc-w --cpus 6 --memory 6G --net-default deny \
             --net-rule allow@dns --net-rule allow@github.com:tcp:443 \
             --net-rule allow@*.githubusercontent.com:tcp:443 --net-strict=false \
             --mount-dir /w:/w:uid=1000,gid=1000 --mount-named abc-w-target:/w/target:kind=disk,size=40G \
             --tmpfs /run/abc --vsock /h/b.sock:5000 --env ABC_VM=1 abc-agent:latest"
        );
    }

    #[test]
    fn keeps_its_state_per_version() {
        assert_eq!(
            home(Path::new("/d"), "0.7.13"),
            Path::new("/d/abc/msb/0.7.13")
        );
    }

    /// A fake msb in `dir` that logs its arguments and `MSB_HOME`, and answers `ls`.
    fn fake(dir: &Path, body: &str) -> Msb {
        let exe = dir.join("msb");
        script(
            &exe,
            &format!(
                "echo \"$MSB_HOME $*\" >> {log}\n{body}",
                log = dir.join("log").display()
            ),
        );
        Msb {
            exe,
            home: dir.join("home"),
        }
    }

    fn log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("log")).unwrap_or_default()
    }

    #[test]
    fn drives_msb_with_its_own_home() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(
            dir.path(),
            "case $1 in\n  ls) printf 'NAME IMAGE STATUS\\nabc-w img running\\n' ;;\n  exec) cat; echo \" out\"; echo err >&2; exit 3 ;;\nesac\n",
        );
        assert!(msb.is_running("abc-w").unwrap());
        assert!(!msb.is_running("abc").unwrap(), "whole names only");
        let output = msb
            .exec("abc-w", &[OsString::from("cat")], b"secret-in")
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "secret-in out\n");
        assert_eq!(String::from_utf8(output.stderr).unwrap(), "err\n");
        assert_eq!(output.status.code(), Some(3));
        msb.start(&spec()).unwrap();
        assert!(msb.list().unwrap().contains("abc-w"));
        let home = dir.path().join("home");
        let log = log(dir.path());
        assert!(
            log.contains(&format!("{} exec abc-w -- cat\n", home.display())),
            "{log}"
        );
        assert!(
            log.contains(&format!("{} rm --force abc-w\n", home.display())),
            "{log}"
        );
        assert!(log.contains(" run --detach --quiet --name abc-w "), "{log}");
        assert!(!log.contains("secret-in"), "stdin is not an argument");
    }

    #[test]
    fn reports_msb_failures() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(dir.path(), "echo 'boom' >&2\nexit 1\n");
        let error = format!("{:#}", msb.list().unwrap_err());
        assert_eq!(error, "msb ls failed: boom");
        assert!(msb.start(&spec()).is_err());
        assert!(msb.remove("abc-w").is_err());
        assert!(msb.remove_volumes(&["v".to_owned()]).is_err());
        let load = format!("{:#}", msb.load_image(&mut &b"tar"[..], "t").unwrap_err());
        assert_eq!(load, "msb load failed: boom");
        let missing = Msb {
            exe: dir.path().join("none"),
            home: dir.path().to_path_buf(),
        };
        assert!(format!("{:#}", missing.list().unwrap_err()).contains("could not run"));
        missing.exec("a", &[], b"").unwrap_err();
        missing.attach("a", &[]).unwrap_err();
        assert!(missing.load_image(&mut &b""[..], "t").is_err());
    }

    #[test]
    fn removing_what_is_gone_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(dir.path(), "echo 'error: sandbox not found' >&2\nexit 1\n");
        msb.remove("abc-w").unwrap();
        msb.remove_volumes(&["abc-w-target".to_owned(), "abc-w-home".to_owned()])
            .unwrap();
        let log = log(dir.path());
        assert!(
            log.contains("volume rm abc-w-target\n") && log.contains("volume rm abc-w-home\n"),
            "{log}"
        );
    }

    #[test]
    fn loads_an_image_from_a_stream_and_attaches_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(
            dir.path(),
            &format!(
                "case $1 in\n  load) cat > {} ;;\n  exec) exit 7 ;;\nesac\n",
                dir.path().join("loaded").display()
            ),
        );
        msb.load_image(&mut &b"oci archive"[..], "abc-agent:latest")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("loaded")).unwrap(),
            "oci archive"
        );
        let status = msb.attach("abc-w", &[OsString::from("claude")]).unwrap();
        assert_eq!(status.code(), Some(7));
        let log = log(dir.path());
        assert!(
            log.contains("load --quiet --tag abc-agent:latest\n"),
            "{log}"
        );
        assert!(log.contains("exec --tty abc-w -- claude\n"), "{log}");
    }
}
