//! `cargo xtask coverage`: how many changed, instrumented lines the tests run.
//!
//! cargo-llvm-cov runs the test suite with coverage instrumentation and writes lcov, and Vitest
//! does the same for the web app's tests (`web/`), with coverage-v8. The gate reads both and the
//! diff from the merge base, and fails when fewer than [`THRESHOLD_PERCENT`]
//! of the changed lines that compile to code are covered, listing the ones that are not; in GitHub
//! Actions, also as annotations, one per range of lines.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};

use crate::annotation::{self, Annotation, Mode};
use crate::check::{self, PROPTEST_RNG_SEED};
use crate::git;
use crate::integrity::{self, TestCode};
use crate::tools;
use crate::web;

/// The share of changed, instrumented lines the tests must run.
pub(crate) const THRESHOLD_PERCENT: u64 = 90;

/// Where cargo-llvm-cov writes the lcov reports, one per workspace, relative to the repository
/// root.
const LCOV_DIR: &str = "target/coverage";

/// Hit counts by line, by file relative to the repository root.
type Hits = BTreeMap<String, BTreeMap<u32, u64>>;

/// The pinned tools coverage runs.
#[derive(Debug)]
pub(crate) struct Tools {
    pub(crate) llvm_cov: PathBuf,
    pub(crate) nextest: PathBuf,
    /// node and pnpm, for the web app's tests.
    pub(crate) web: web::Toolchain,
}

/// Runs the tests of every workspace and of the web app under coverage, with the tools given,
/// and checks the lines changed since the merge base with `base`, annotating the lines no test
/// runs in `mode`.
pub(crate) fn run(
    root: &Path,
    base: &str,
    tools: &Tools,
    mode: Mode,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    let dir = root.join(LCOV_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    if crate::postgres::needed(root) {
        crate::postgres::warm(out)?;
    }
    // The abc crate embeds the built web app.
    ensure!(
        web::build_here(root, &tools.web, out)?,
        "the web app did not build; run `cargo xtask check` and fix it first"
    );
    let (llvm_cov, nextest) = (&tools.llvm_cov, &tools.nextest);
    let mut report = String::new();
    for &workspace in check::REPOSITORY.workspaces {
        let name = if workspace.dir().is_empty() {
            "main"
        } else {
            workspace.dir()
        };
        let lcov = dir.join(format!("{name}.lcov.info"));
        writeln!(
            out,
            "xtask: running the tests of the {name} workspace under coverage"
        )?;
        let junit = (mode == Mode::Actions)
            .then(|| crate::junit::prepare(root, &workspace.manifest_args(), "llvm-cov-target"));
        let status = Command::new(llvm_cov)
            .args(["llvm-cov", "nextest"])
            .args(workspace.manifest_args())
            .args(["--workspace", "--locked", "--lcov"])
            .arg("--output-path")
            .arg(&lcov)
            .env("PATH", tools::path_with(nextest)?)
            .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED)
            .current_dir(root)
            .status()
            .context("could not run cargo-llvm-cov")?;
        if !status.success()
            && let Some(junit) = &junit
        {
            let failed = crate::junit::annotations_of(junit, workspace.dir(), "coverage's tests");
            write!(out, "{}", annotation::commands(mode, &failed))?;
        }
        ensure!(
            status.success(),
            "the tests of the {name} workspace failed under coverage; run `cargo xtask check` and \
             fix them first"
        );
        report.push_str(
            &fs::read_to_string(&lcov)
                .with_context(|| format!("could not read {}", lcov.display()))?,
        );
    }
    if web::present(root) {
        report.push_str(&web_report(root, &tools.web, out)?);
    }
    let commit = git::merge_base(root, base)?;
    let changed = without_tests(root, git::added_lines(&git::diff(root, &commit, 0)?))?;
    let measured = measure(&changed, &parse_lcov(&report, root));
    write!(out, "{}", measured.describe())?;
    write!(
        out,
        "{}",
        annotation::commands(mode, &measured.annotations())
    )?;
    Ok(measured.passes())
}

/// Runs the web app's tests under coverage and returns their lcov, with paths made absolute as
/// cargo-llvm-cov's are.
fn web_report(
    root: &Path,
    toolchain: &web::Toolchain,
    out: &mut impl Write,
) -> anyhow::Result<String> {
    writeln!(
        out,
        "xtask: running the tests of the web app under coverage"
    )?;
    let status = toolchain
        .step(root, &web::COVERAGE)?
        .status()
        .context("could not run pnpm")?;
    ensure!(
        status.success(),
        "the tests of the web app failed under coverage; run `cargo xtask check` and fix them first"
    );
    let dir = root.join(web::DIR);
    let lcov = dir.join(web::LCOV);
    let text =
        fs::read_to_string(&lcov).with_context(|| format!("could not read {}", lcov.display()))?;
    Ok(absolute_sources(&text, &dir))
}

/// `report` with each relative source path, as Vitest writes them, made absolute under `dir`; an
/// absolute one stays as it is.
fn absolute_sources(report: &str, dir: &Path) -> String {
    report
        .lines()
        .map(|line| match line.strip_prefix("SF:") {
            Some(path) => format!("SF:{}\n", dir.join(path).display()),
            None => format!("{line}\n"),
        })
        .collect()
}

/// `changed` without test-only code: test items, test modules in files of their own, and files
/// under a `tests` directory. The gate measures how well tests cover shipped code, not the tests.
fn without_tests(
    root: &Path,
    changed: BTreeMap<String, BTreeSet<u32>>,
) -> anyhow::Result<BTreeMap<String, BTreeSet<u32>>> {
    let test_code = |path: &Path| -> anyhow::Result<Option<TestCode>> {
        let Ok(source) = fs::read_to_string(root.join(path)) else {
            return Ok(None);
        };
        integrity::test_code(path, &source)
            .transpose()
            .with_context(|| format!("could not read the tests in {}", path.display()))
    };
    let mut kept = BTreeMap::new();
    for (file, mut lines) in changed {
        let path = Path::new(&file);
        if path.components().any(|part| part.as_os_str() == "tests") {
            continue;
        }
        if let Some(code) = test_code(path)? {
            lines.retain(|line| !code.lines.contains(line));
        }
        let mut declared_as_test = false;
        for (name, declarer) in declarers(path) {
            if test_code(&declarer)?.is_some_and(|code| code.modules.contains(&name)) {
                declared_as_test = true;
            }
        }
        if !declared_as_test && !lines.is_empty() {
            kept.insert(file, lines);
        }
    }
    Ok(kept)
}

/// The files that may declare the module in `path`, with the name they declare it by.
fn declarers(path: &Path) -> Vec<(String, PathBuf)> {
    let (Some(stem), Some(dir)) = (path.file_stem(), path.parent()) else {
        return Vec::new();
    };
    let (name, home) = if stem == "mod" {
        match (dir.file_name(), dir.parent()) {
            (Some(name), Some(parent)) => (name, parent),
            _ => return Vec::new(),
        }
    } else {
        (stem, dir)
    };
    let name = name.to_string_lossy().into_owned();
    let sibling = match (home.file_name(), home.parent()) {
        (Some(module), Some(parent)) => {
            Some(parent.join(format!("{}.rs", module.to_string_lossy())))
        }
        _ => None,
    };
    ["main.rs", "lib.rs", "mod.rs"]
        .iter()
        .map(|file| home.join(file))
        .chain(sibling)
        .filter(|file| file != path)
        .map(|file| (name.clone(), file))
        .collect()
}

/// Reads an lcov report into hit counts, with paths relative to `root`. Files outside it, such as
/// dependencies, are left out.
fn parse_lcov(report: &str, root: &Path) -> Hits {
    let mut hits = Hits::new();
    let mut file: Option<String> = None;
    for line in report.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            file = Path::new(path)
                .strip_prefix(root)
                .ok()
                .map(|relative| relative.to_string_lossy().into_owned());
        } else if line == "end_of_record" {
            file = None;
        } else if let (Some(path), Some(data)) = (&file, line.strip_prefix("DA:")) {
            let mut fields = data.split(',');
            let number = fields.next().and_then(|field| field.parse::<u32>().ok());
            let count = fields.next().and_then(|field| field.parse::<u64>().ok());
            if let (Some(number), Some(count)) = (number, count) {
                let entry = hits
                    .entry(path.clone())
                    .or_default()
                    .entry(number)
                    .or_insert(0);
                *entry = (*entry).max(count);
            }
        }
    }
    hits
}

/// The coverage of the changed lines.
#[derive(Debug, PartialEq, Eq)]
struct Measured {
    covered: u64,
    instrumented: u64,
    /// Changed, instrumented lines no test ran, by file.
    uncovered: BTreeMap<String, BTreeSet<u32>>,
}

fn measure(changed: &BTreeMap<String, BTreeSet<u32>>, hits: &Hits) -> Measured {
    let mut measured = Measured {
        covered: 0,
        instrumented: 0,
        uncovered: BTreeMap::new(),
    };
    for (file, lines) in changed {
        let Some(counts) = hits.get(file) else {
            continue;
        };
        for line in lines {
            let Some(&count) = counts.get(line) else {
                continue;
            };
            measured.instrumented = measured.instrumented.saturating_add(1);
            if count > 0 {
                measured.covered = measured.covered.saturating_add(1);
            } else {
                measured
                    .uncovered
                    .entry(file.clone())
                    .or_default()
                    .insert(*line);
            }
        }
    }
    measured
}

impl Measured {
    fn passes(&self) -> bool {
        self.covered.saturating_mul(100) >= self.instrumented.saturating_mul(THRESHOLD_PERCENT)
    }

    fn describe(&self) -> String {
        let percent = self
            .covered
            .saturating_mul(100)
            .checked_div(self.instrumented)
            .unwrap_or(100);
        let mut text = format!(
            "coverage: {} of {} changed, instrumented lines are covered ({percent}%); the gate \
             needs {THRESHOLD_PERCENT}%\n",
            self.covered, self.instrumented
        );
        for (file, lines) in &self.uncovered {
            // Writing to a String cannot fail.
            if writeln!(text, "  not covered: {file}:{}", ranges(lines)).is_err() {
                break;
            }
        }
        let verdict = if self.passes() {
            "coverage: passed\n"
        } else {
            "coverage: failed; add tests that run the lines above\n"
        };
        text.push_str(verdict);
        text
    }

    /// One annotation per range of changed lines no test runs.
    fn annotations(&self) -> Vec<Annotation> {
        self.uncovered
            .iter()
            .flat_map(|(file, lines)| {
                spans(lines).into_iter().map(move |(first, last)| {
                    let message = if first == last {
                        format!("Line {first} changed and no test runs it.")
                    } else {
                        format!("Lines {first}-{last} changed and no test runs them.")
                    };
                    Annotation::error(file, "Not covered", message)
                        .at(u64::from(first), u64::from(last))
                })
            })
            .collect()
    }
}

/// Lines as runs of consecutive numbers, each its first and last.
fn spans(lines: &BTreeSet<u32>) -> Vec<(u32, u32)> {
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for &line in lines {
        match spans.last_mut() {
            Some((_, end)) if end.checked_add(1) == Some(line) => *end = line,
            _ => spans.push((line, line)),
        }
    }
    spans
}

/// Lines as comma-separated numbers and ranges, such as `3, 7-9`.
fn ranges(lines: &BTreeSet<u32>) -> String {
    spans(lines)
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::{
        Measured, Tools, absolute_sources, measure, parse_lcov, ranges, run, without_tests,
    };
    use crate::annotation::Mode;
    use crate::testing::{repository_with_change, script};

    const LCOV: &str = "TN:\nSF:/repo/src/a.rs\nFN:1,a\nDA:1,3\nDA:2,0\nDA:3,0\nDA:4,1\nDA:4,0\nend_of_record\nSF:/elsewhere/dep.rs\nDA:1,0\nend_of_record\nSF:/repo/src/b.rs\nDA:7,0,abc\nend_of_record\n";

    fn lines(numbers: &[u32]) -> BTreeSet<u32> {
        numbers.iter().copied().collect()
    }

    /// The tools for a fixture with fakes for cargo-llvm-cov and cargo-nextest; fixtures hold no
    /// web app, so node and pnpm are never run.
    fn tools(llvm_cov: &Path, nextest: &Path) -> Tools {
        Tools {
            llvm_cov: llvm_cov.to_path_buf(),
            nextest: nextest.to_path_buf(),
            web: crate::web::Toolchain::pinned(&|name| Ok(Path::new("unused").join(name))).unwrap(),
        }
    }

    #[test]
    fn makes_the_web_apps_sources_absolute() {
        let vitest =
            "TN:\nSF:src/app.tsx\nDA:1,1\nend_of_record\nSF:/elsewhere/a.ts\nend_of_record\n";
        let absolute = absolute_sources(vitest, Path::new("/repo/web"));
        assert_eq!(
            absolute,
            "TN:\nSF:/repo/web/src/app.tsx\nDA:1,1\nend_of_record\nSF:/elsewhere/a.ts\nend_of_record\n",
            "relative paths are made absolute, absolute ones kept"
        );
        assert_eq!(
            parse_lcov(&absolute, Path::new("/repo")).get("web/src/app.tsx"),
            Some(&BTreeMap::from([(1, 1)])),
            "counted under web/ with the Rust sources"
        );
    }

    #[test]
    fn reads_lcov_relative_to_the_repository() {
        let hits = parse_lcov(LCOV, Path::new("/repo"));
        assert_eq!(
            hits.get("src/a.rs"),
            Some(&BTreeMap::from([(1, 3), (2, 0), (3, 0), (4, 1)])),
            "a line listed twice keeps its highest count"
        );
        assert_eq!(
            hits.get("src/b.rs"),
            Some(&BTreeMap::from([(7, 0)])),
            "a checksum field"
        );
        assert_eq!(hits.len(), 2, "files outside the repository are left out");
    }

    #[test]
    fn measures_only_changed_instrumented_lines() {
        let hits = parse_lcov(LCOV, Path::new("/repo"));
        let changed = BTreeMap::from([
            ("src/a.rs".to_owned(), lines(&[1, 2, 3, 5])),
            ("README.md".to_owned(), lines(&[1])),
        ]);
        assert_eq!(
            measure(&changed, &hits),
            Measured {
                covered: 1,
                instrumented: 3,
                uncovered: BTreeMap::from([("src/a.rs".to_owned(), lines(&[2, 3]))]),
            },
            "line 5 compiles to no code and README.md is not instrumented"
        );
    }

    fn measured(covered: u64, instrumented: u64) -> Measured {
        Measured {
            covered,
            instrumented,
            uncovered: BTreeMap::new(),
        }
    }

    #[test]
    fn the_threshold_is_ninety_percent() {
        assert!(measured(9, 10).passes(), "exactly 90% passes");
        assert!(!measured(899, 1000).passes(), "89.9% fails");
        assert!(measured(0, 0).passes(), "no instrumented change passes");
        assert!(!measured(0, 1).passes(), "one uncovered line fails");
    }

    #[test]
    fn describes_the_result() {
        let mut failing = measured(1, 3);
        failing
            .uncovered
            .insert("src/a.rs".to_owned(), lines(&[2, 3, 9]));
        let text = failing.describe();
        assert!(
            text.contains("1 of 3 changed, instrumented lines are covered (33%)"),
            "{text}"
        );
        assert!(text.contains("  not covered: src/a.rs:2-3, 9\n"), "{text}");
        assert!(
            text.ends_with("coverage: failed; add tests that run the lines above\n"),
            "{text}"
        );
        assert_eq!(
            crate::annotation::commands(Mode::Actions, &failing.annotations()),
            "::error file=src/a.rs,line=2,endLine=3,title=Not covered::Lines 2-3 changed and no \
             test runs them.\n::error file=src/a.rs,line=9,title=Not covered::Line 9 changed and \
             no test runs it.\n",
            "one annotation per range"
        );
        assert!(
            measured(0, 0).describe().contains("(100%)"),
            "nothing to cover"
        );
        assert!(
            measured(2, 2).describe().ends_with("coverage: passed\n"),
            "passing"
        );
    }

    /// A fake cargo-llvm-cov that writes an lcov report covering line 1 of `src/a.rs` and, when
    /// `both` is set, line 2 too.
    fn fake_llvm_cov(dir: &Path, both: bool) -> std::path::PathBuf {
        let path = dir.join("cargo-llvm-cov");
        let second = u8::from(both);
        script(
            &path,
            &format!(
                "[ \"$1 $2\" = 'llvm-cov nextest' ] || exit 7\nwhile [ $# -gt 0 ]; do [ \"$1\" = --output-path ] && out=$2; shift; done\nprintf 'SF:%s/src/a.rs\\nDA:1,3\\nDA:2,{second}\\nend_of_record\\n' \"$PWD\" > \"$out\"\n"
            ),
        );
        path
    }

    #[test]
    fn leaves_test_code_out() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let files = [
            (
                "src/main.rs",
                "mod a;\n#[cfg(test)]\nmod testing;\n#[cfg(test)]\nmod inner;\nfn main() {}\n",
            ),
            ("src/testing.rs", "pub fn helper() {}\n"),
            ("src/inner/mod.rs", "pub fn nested() {}\n"),
            ("src/a/deep.rs", "pub fn deep() {}\n"),
            (
                "src/a.rs",
                "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n#[cfg(test)]\nmod deep;\n",
            ),
            ("tests/it.rs", "#[test]\nfn it() {}\n"),
            ("notes.txt", "x\n"),
        ];
        let mut changed = BTreeMap::new();
        for (file, content) in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
            let lines = u32::try_from(content.lines().count()).unwrap();
            changed.insert(file.to_owned(), (1..=lines).collect::<BTreeSet<u32>>());
        }
        changed.insert("src/gone.rs".to_owned(), BTreeSet::from([1]));
        let kept = without_tests(root, changed).unwrap();
        assert_eq!(
            kept.keys().map(String::as_str).collect::<Vec<_>>(),
            ["notes.txt", "src/a.rs", "src/gone.rs", "src/main.rs"],
            "test modules in their own files and files under tests/ are gone"
        );
        assert_eq!(
            kept["src/a.rs"].iter().copied().collect::<Vec<_>>(),
            [1, 2],
            "the test modules' lines are gone"
        );
        assert_eq!(
            kept["src/main.rs"].iter().copied().collect::<Vec<_>>(),
            [1, 6],
            "the test module declarations are test code too"
        );
        std::fs::write(root.join("src/broken.rs"), "fn (").unwrap();
        assert!(
            without_tests(
                root,
                BTreeMap::from([("src/broken.rs".to_owned(), BTreeSet::from([1]))])
            )
            .is_err(),
            "a file the check cannot read fails the gate"
        );
    }

    #[test]
    fn gates_a_repository_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("src/a.rs", "fn a() {}\nfn b() {}\n")],
        );
        let nextest = temp.path().join("cargo-nextest");
        let mut out = Vec::new();
        let half = run(
            &repo,
            "base",
            &tools(&fake_llvm_cov(temp.path(), false), &nextest),
            Mode::Actions,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!half, "half the changed lines covered fails: {text}");
        assert!(text.contains("not covered: src/a.rs:2"), "{text}");
        assert!(
            text.ends_with(
                "coverage: failed; add tests that run the lines above\n::error \
                 file=src/a.rs,line=2,title=Not covered::Line 2 changed and no test runs it.\n"
            ),
            "in GitHub Actions the lines are annotated too: {text}"
        );
        let full = run(
            &repo,
            "base",
            &tools(&fake_llvm_cov(temp.path(), true), &nextest),
            Mode::Plain,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(full, "every changed line covered passes");
        let failing = temp.path().join("failing");
        script(&failing, "exit 1\n");
        let error = run(
            &repo,
            "base",
            &tools(&failing, &nextest),
            Mode::Plain,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the tests of the main workspace failed under coverage"),
            "{error}"
        );
    }

    /// #33: a test that failed under coverage showed only "exit code 1" in the checks; inside
    /// GitHub Actions it is now an annotation at its panic.
    #[test]
    fn annotates_the_tests_that_fail_under_coverage() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"xtask\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                ("src/main.rs", "fn main() {}\n"),
            ],
        );
        let locked = std::process::Command::new(crate::check::cargo())
            .arg("generate-lockfile")
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(locked.success());
        let report = crate::junit::target_directory(&repo, &[])
            .unwrap()
            .join("llvm-cov-target")
            .join(crate::junit::REPORT);
        std::fs::create_dir_all(report.parent().unwrap()).unwrap();
        std::fs::write(&report, "stale: an earlier run's report is never read").unwrap();
        let recorded = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/nextest-junit-33.xml");
        let failing = temp.path().join("failing");
        script(
            &failing,
            &format!(
                "[ -e {report} ] && exit 7\ncp {recorded} {report}\nexit 1\n",
                report = report.display(),
                recorded = recorded.display()
            ),
        );
        let mut out = Vec::new();
        let error = run(
            &repo,
            "base",
            &tools(&failing, &temp.path().join("cargo-nextest")),
            Mode::Actions,
            &mut out,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("failed under coverage"),
            "{error}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "::error file=src/broker.rs,line=6,title=Test failed%3A xtask%3A%3Abin/xtask \
                 broker%3A%3Atests%3A%3Aa_client_has_its_patience_in_all_not_per_read::"
            ),
            "{text}"
        );
    }

    #[test]
    fn counts_the_web_apps_changed_lines_with_the_rust_ones() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("web/package.json", "{}\n"), ("web/src/a.ts", "a;\nb;\n")],
        );
        let empty = temp.path().join("cargo-llvm-cov");
        script(
            &empty,
            "while [ $# -gt 0 ]; do [ \"$1\" = --output-path ] && out=$2; shift; done\n: > \"$out\"\n",
        );
        let nextest = temp.path().join("cargo-nextest");
        let web = temp.path().join("web-tools");
        let tools = Tools {
            llvm_cov: empty.clone(),
            nextest: nextest.clone(),
            web: crate::testing::fake_web(&web, ""),
        };
        let mut out = Vec::new();
        let passed = run(&repo, "base", &tools, Mode::Plain, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!passed, "half the web lines covered fails: {text}");
        assert!(text.contains("not covered: web/src/a.ts:2"), "{text}");
        assert!(
            crate::testing::pnpm_calls(&web).ends_with("run --silent coverage\n"),
            "built first, then Vitest under coverage"
        );

        let failing = Tools {
            llvm_cov: empty,
            nextest,
            web: crate::testing::fake_web(&temp.path().join("failing"), "coverage"),
        };
        let error = run(&repo, "base", &failing, Mode::Plain, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the tests of the web app failed under coverage"),
            "{error}"
        );
        let unbuilt = Tools {
            llvm_cov: temp.path().join("unused"),
            nextest: temp.path().join("unused"),
            web: crate::testing::fake_web(&temp.path().join("unbuilt"), "build"),
        };
        let unbuilt_error = run(&repo, "base", &unbuilt, Mode::Plain, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            unbuilt_error.contains("the web app did not build"),
            "{unbuilt_error}"
        );
    }

    #[test]
    fn measures_the_xtask_workspace_from_its_own_run() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository_with_change(
            &temp.path().join("repo"),
            &[("xtask/src/b.rs", "fn b() {}\nfn c() {}\n")],
        );
        let nextest = temp.path().join("cargo-nextest");
        let llvm_cov = temp.path().join("cargo-llvm-cov");
        script(
            &llvm_cov,
            "manifest=main\nwhile [ $# -gt 0 ]; do case \"$1\" in --output-path) out=$2 ;; --manifest-path) manifest=$2 ;; esac; shift; done\nif [ \"$manifest\" = xtask/Cargo.toml ]; then printf 'SF:%s/xtask/src/b.rs\\nDA:1,1\\nDA:2,0\\nend_of_record\\n' \"$PWD\" > \"$out\"; else printf 'SF:%s/src/a.rs\\nDA:1,1\\nend_of_record\\n' \"$PWD\" > \"$out\"; fi\n",
        );
        let mut out = Vec::new();
        let passed = run(
            &repo,
            "base",
            &tools(&llvm_cov, &nextest),
            Mode::Plain,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!passed, "xtask's uncovered line counts: {text}");
        assert!(
            !text.contains("::error"),
            "no annotations outside GitHub Actions"
        );
        assert!(text.contains("not covered: xtask/src/b.rs:2"), "{text}");
        assert!(
            text.contains("the tests of the main workspace")
                && text.contains("the tests of the xtask workspace"),
            "{text}"
        );
        for report in ["main", "xtask"] {
            assert!(
                repo.join(format!("target/coverage/{report}.lcov.info"))
                    .is_file(),
                "a report per workspace: {report}"
            );
        }
    }

    #[test]
    fn writes_line_ranges() {
        assert_eq!(
            ranges(&lines(&[1, 2, 3, 5, 7, 8])),
            "1-3, 5, 7-8",
            "runs collapse"
        );
        assert_eq!(ranges(&lines(&[4])), "4", "a single line");
    }
}
