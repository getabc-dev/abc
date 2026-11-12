//! The microsandbox backend: `msb`, pinned in `xtask/tools.toml`, run with an `MSB_HOME` of its
//! own per version, since microsandbox is beta and its releases break compatibility.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

use anyhow::{Context as _, ensure};

use super::{Backend, Sandbox, Spec};

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
    // Deny by default; HTTPS to the allowed hosts, matched by the TLS server name (without
    // intercepting TLS, which would put a CA of microsandbox's in the guest). With no DNS rule,
    // microsandbox's resolver answers only for the allowed hosts.
    args.extend(["--net-default", "deny"].map(OsString::from));
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
        Ok(self.list(true)?.iter().any(|sandbox| sandbox.name == name))
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
        // A command that exits without reading its input still reports its own outcome.
        crate::tools::feed(&mut child, &mut &stdin[..])
            .context("could not write to the command in the VM")?;
        child
            .wait_with_output()
            .context("could not wait for the command in the VM")
    }

    fn attach(&self, name: &str, command: &[OsString], tty: bool) -> anyhow::Result<ExitStatus> {
        let mut exec = self.command();
        exec.arg("exec");
        if tty {
            exec.arg("--tty");
        }
        // Inherited stdin, stdout and stderr pass straight through, as does the exit status.
        exec.arg(name)
            .arg("--")
            .args(command)
            .status()
            .with_context(|| format!("could not run {}", self.exe.display()))
    }

    /// `msb exec --stream`: without it, msb keeps a command's output that does not go to a
    /// terminal in memory until the command exits, so a log stayed empty for hours (microsandbox
    /// 0.7.13, `crates/cli/lib/commands/exec.rs`); with it, msb writes and flushes each chunk as
    /// it comes. `--no-stdin` gives the command no input, which `--stream` needs.
    fn stream(
        &self,
        name: &str,
        command: &[OsString],
        log: &mut dyn Write,
    ) -> anyhow::Result<ExitStatus> {
        let mut exec = self.command();
        exec.args(["exec", "--stream", "--no-stdin"])
            .arg(name)
            .arg("--")
            .args(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = exec
            .spawn()
            .with_context(|| format!("could not run {}", self.exe.display()))?;
        let stdout = child.stdout.take().context("the command has no stdout")?;
        let stderr = child.stderr.take().context("the command has no stderr")?;
        // Each stream in its own pipe, read a whole line at a time: a line of one is never cut by
        // the other, as when stderr is written while a long stream-json line is on its way.
        let (lines, received) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
        std::thread::scope(|scope| -> anyhow::Result<()> {
            let streams: [Box<dyn Read + Send>; 2] = [Box::new(stdout), Box::new(stderr)];
            for stream in streams {
                let lines = lines.clone();
                scope.spawn(move || {
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut line = Vec::new();
                        match reader.read_until(b'\n', &mut line) {
                            Ok(0) => return,
                            Ok(_) => {
                                if lines.send(Ok(line)).is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                let _sent = lines.send(Err(error));
                                return;
                            }
                        }
                    }
                });
            }
            drop(lines);
            for line in received {
                let line = line.context("could not read the command's output")?;
                log.write_all(&line)
                    .and_then(|()| log.flush())
                    .context("could not write the log")?;
            }
            Ok(())
        })?;
        child
            .wait()
            .context("could not wait for the command in the VM")
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

    /// The volumes under msb's home, each a directory named for it, and the blocks their files
    /// take: their disks are sparse, so their sizes overstate it.
    fn volumes(&self) -> anyhow::Result<Vec<(String, u64)>> {
        let dir = self.home.join("volumes");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", dir.display()));
            }
        };
        let mut volumes = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("could not read {}", dir.display()))?;
            volumes.push((
                entry.file_name().to_string_lossy().into_owned(),
                disk_usage(&entry.path())?,
            ));
        }
        volumes.sort();
        Ok(volumes)
    }

    /// `msb ls --format json`: its human listing goes to stderr, where a script cannot read it,
    /// and its JSON to stdout.
    fn list(&self, running_only: bool) -> anyhow::Result<Vec<Sandbox>> {
        let mut args: Vec<OsString> = ["ls", "--format", "json"].map(OsString::from).into();
        if running_only {
            args.push("--running".into());
        }
        let listing = self.succeed(&args)?;
        serde_json::from_str(&listing)
            .with_context(|| format!("msb ls --format json answered something else: {listing}"))
    }

    fn load_image(&self, archive: &mut dyn Read, tag: &str) -> anyhow::Result<()> {
        let mut child = self
            .command()
            .args(["load", "--quiet", "--tag", tag])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run {}", self.exe.display()))?;
        let taken = crate::tools::feed(&mut child, archive)
            .context("could not pass the image to msb load")?;
        let output = child
            .wait_with_output()
            .context("could not wait for msb load")?;
        ensure!(
            output.status.success(),
            "msb load failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        ensure!(taken, "msb load exited before reading the whole image");
        Ok(())
    }
}

/// The bytes the files under `path` take on disk, counted in blocks, without following links.
fn disk_usage(path: &Path) -> anyhow::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let mut bytes = metadata.blocks().saturating_mul(512);
    if metadata.is_dir() {
        for entry in
            std::fs::read_dir(path).with_context(|| format!("could not read {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("could not read {}", path.display()))?;
            bytes = bytes.saturating_add(disk_usage(&entry.path())?);
        }
    }
    Ok(bytes)
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
    use crate::vm::Sandbox;
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
             --net-rule allow@github.com:tcp:443 \
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
            "case $1 in\n  ls) echo 'a table, as a person reads it' >&2; echo '[{\"name\":\"abc-w\",\"status\":\"Running\",\"image\":\"img\"}]' ;;\n  exec) cat; echo \" out\"; echo err >&2; exit 3 ;;\nesac\n",
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
        assert_eq!(
            msb.list(false).unwrap(),
            [Sandbox {
                name: "abc-w".to_owned(),
                status: "Running".to_owned()
            }],
            "the JSON on stdout, and the fields xtask does not read left out"
        );
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

    /// What `msb ls` does, which the fake of it in [`fake`] and the backend fake in `vm/tests.rs`
    /// both model, checked against the pinned msb itself: with no VM, `ls` ends with 0 and says
    /// "No sandboxes found." on stderr, where a human listing goes, and `ls --format json` prints
    /// `[]` on stdout and nothing on stderr; `--running` changes nothing for no VM. (`msb ls`
    /// needs no KVM, so this runs anywhere msb does.)
    fn assert_listing_contract(msb: &Msb) {
        assert_eq!(msb.list(false).unwrap(), Vec::<Sandbox>::new());
        assert_eq!(msb.list(true).unwrap(), Vec::<Sandbox>::new());
        assert!(!msb.is_running("abc-w").unwrap());
        let human = msb.run(&["ls".into()]).unwrap();
        assert!(human.status.success());
        assert_eq!(
            String::from_utf8_lossy(&human.stdout),
            "",
            "a human listing is never on stdout"
        );
        assert!(
            String::from_utf8_lossy(&human.stderr).contains("No sandboxes found."),
            "{}",
            String::from_utf8_lossy(&human.stderr)
        );
        let json = msb
            .run(&["ls".into(), "--format".into(), "json".into()])
            .unwrap();
        assert!(json.status.success());
        assert_eq!(String::from_utf8_lossy(&json.stdout).trim(), "[]");
        assert_eq!(String::from_utf8_lossy(&json.stderr), "");
    }

    #[test]
    fn a_listing_means_the_same_to_the_fake_and_to_the_real_msb() {
        let dir = tempfile::tempdir().unwrap();
        let real = Msb {
            exe: crate::testing::pinned_for_tests("msb"),
            home: dir.path().join("real"),
        };
        assert_listing_contract(&real);
        let fake_dir = dir.path().join("fake");
        std::fs::create_dir(&fake_dir).unwrap();
        let fake = fake(
            &fake_dir,
            "case $1 in\n  ls) case \"$2\" in --format) echo '[]' ;; *) echo 'No sandboxes found.' >&2 ;; esac ;; esac\n",
        );
        assert_listing_contract(&fake);
    }

    #[test]
    fn reports_msb_failures() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(dir.path(), "echo 'boom' >&2\nexit 1\n");
        let error = format!("{:#}", msb.list(false).unwrap_err());
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
        assert!(format!("{:#}", missing.list(false).unwrap_err()).contains("could not run"));
        missing.exec("a", &[], b"").unwrap_err();
        missing.attach("a", &[], false).unwrap_err();
        assert!(missing.load_image(&mut &b""[..], "t").is_err());
    }

    #[test]
    fn a_child_that_stops_reading_reports_its_own_outcome() {
        // A megabyte overflows any pipe buffer, so the write meets the closed pipe for certain.
        let big = vec![b'x'; 1 << 20];
        let dir = tempfile::tempdir().unwrap();
        let failing = fake(dir.path(), "echo 'bad archive' >&2\nexit 1\n");
        let failed = format!("{:#}", failing.load_image(&mut &big[..], "t").unwrap_err());
        assert_eq!(failed, "msb load failed: bad archive");
        let output = failing.exec("abc-w", &[], &big).unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "the command's status, not EPIPE"
        );
        let quitting = fake(dir.path(), "exit 0\n");
        let short = format!("{:#}", quitting.load_image(&mut &big[..], "t").unwrap_err());
        assert_eq!(short, "msb load exited before reading the whole image");
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
        let status = msb
            .attach("abc-w", &[OsString::from("claude")], true)
            .unwrap();
        assert_eq!(status.code(), Some(7));
        let headless = msb
            .attach("abc-w", &[OsString::from("claude"), "-p".into()], false)
            .unwrap();
        assert_eq!(headless.code(), Some(7), "the exit status passes through");
        let log = log(dir.path());
        assert!(
            log.contains("load --quiet --tag abc-agent:latest\n"),
            "{log}"
        );
        assert!(log.contains("exec --tty abc-w -- claude\n"), "{log}");
        assert!(
            log.contains("exec abc-w -- claude -p\n"),
            "no --tty without a terminal: {log}"
        );
    }

    #[test]
    /// Review finding: stdout and stderr shared one pipe, so stderr written while a long
    /// stdout line was on its way landed inside it, and the run's stream-json result no longer
    /// parsed. Each stream's lines reach the log whole.
    fn a_line_of_stdout_is_never_cut_by_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(
            dir.path(),
            "printf '{\"result\":\"'\nhead -c 100000 /dev/zero | tr '\\0' a\necho oops >&2\nprintf '\"}\\n'\n",
        );
        let mut log = Vec::new();
        msb.stream("abc-w", &[OsString::from("claude")], &mut log)
            .unwrap();
        let text = String::from_utf8(log).unwrap();
        let line = format!("{{\"result\":\"{}\"}}\n", "a".repeat(100_000));
        assert!(text.contains(&line), "the stdout line is whole");
        assert!(text.contains("oops\n"), "stderr reaches the log too");
        assert_eq!(text.len(), line.len() + "oops\n".len());
    }

    /// A session driven from a script once sat for hours with an empty log: without `--stream`
    /// msb held the output back until the command exited. A line reaches the log while the
    /// command still runs.
    #[test]
    fn streams_each_line_to_the_log_while_the_command_runs() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("go");
        let msb = fake(
            dir.path(),
            &format!(
                "echo first\necho oops >&2\nwhile [ ! -e {go} ]; do sleep 0.05; done\nprintf last\nexit 4\n",
                go = go.display()
            ),
        );
        let log_path = dir.path().join("run.log");
        let status = std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                let mut log = std::fs::File::create(&log_path).unwrap();
                msb.stream("abc-w", &[OsString::from("claude"), "-p".into()], &mut log)
                    .unwrap()
            });
            let started = std::time::Instant::now();
            while !std::fs::read_to_string(&log_path)
                .is_ok_and(|text| text.contains("first\n") && text.contains("oops\n"))
            {
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(20),
                    "no line reached the log while the command ran"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(!running.is_finished(), "the command is still running");
            std::fs::write(&go, "").unwrap();
            running.join().unwrap()
        });
        assert_eq!(status.code(), Some(4), "the command's exit status");
        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            text.ends_with("last"),
            "a last line without a newline: {text}"
        );
        assert!(
            log(dir.path()).contains("exec --stream --no-stdin abc-w -- claude -p\n"),
            "{}",
            log(dir.path())
        );
        let missing = Msb {
            exe: dir.path().join("none"),
            home: dir.path().to_path_buf(),
        };
        assert!(
            missing
                .stream("abc-w", &[], &mut Vec::new())
                .is_err_and(|error| error.to_string().contains("could not run"))
        );
    }

    #[test]
    fn measures_each_volume_by_the_blocks_it_takes() {
        let dir = tempfile::tempdir().unwrap();
        let msb = fake(dir.path(), "exit 0\n");
        assert!(msb.volumes().unwrap().is_empty(), "no volumes yet");
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("home/volumes"), "").unwrap();
        assert!(
            msb.volumes()
                .is_err_and(|error| error.to_string().starts_with("could not read ")),
            "a volume directory that cannot be read is an error, not no volumes"
        );
        std::fs::remove_file(dir.path().join("home/volumes")).unwrap();
        let volumes = dir.path().join("home/volumes");
        std::fs::create_dir_all(volumes.join("abc-w-target/nested")).unwrap();
        std::fs::write(
            volumes.join("abc-w-target/nested/disk"),
            vec![7_u8; 1 << 20],
        )
        .unwrap();
        // A sparse disk image: a gigabyte long, nothing written.
        std::fs::File::create(volumes.join("abc-w-home"))
            .unwrap()
            .set_len(1 << 30)
            .unwrap();
        std::os::unix::fs::symlink("/", volumes.join("abc-w-tools")).unwrap();
        let measured = msb.volumes().unwrap();
        let names: Vec<&str> = measured.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["abc-w-home", "abc-w-target", "abc-w-tools"]);
        let bytes = |name: &str| measured.iter().find(|(n, _)| n == name).unwrap().1;
        assert!(bytes("abc-w-target") >= 1 << 20, "{measured:?}");
        assert!(bytes("abc-w-home") < 1 << 20, "sparse: {measured:?}");
        assert!(
            bytes("abc-w-tools") < 1 << 20,
            "a link is not followed: {measured:?}"
        );
    }
}
