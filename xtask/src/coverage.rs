//! `cargo xtask coverage`: how many changed, instrumented lines the tests run.
//!
//! cargo-llvm-cov runs the test suite with coverage instrumentation and writes lcov. The gate
//! reads that and the diff from the merge base, and fails when fewer than [`THRESHOLD_PERCENT`]
//! of the changed lines that compile to code are covered, listing the ones that are not.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, ensure};

use crate::check::PROPTEST_RNG_SEED;
use crate::git;
use crate::tools::{self, Manifest};

/// The share of changed, instrumented lines the tests must run.
pub(crate) const THRESHOLD_PERCENT: u64 = 90;

/// Where cargo-llvm-cov writes the lcov report, relative to the repository root.
const LCOV: &str = "target/coverage/lcov.info";

/// Hit counts by line, by file relative to the repository root.
type Hits = BTreeMap<String, BTreeMap<u32, u64>>;

/// Runs the tests under coverage and checks the lines changed since the merge base with `base`.
pub(crate) fn run(root: &Path, base: &str, out: &mut impl Write) -> anyhow::Result<bool> {
    let manifest = Manifest::load(root)?;
    let llvm_cov = manifest.tool("cargo-llvm-cov")?.installed(root)?;
    let nextest = manifest.tool("cargo-nextest")?.installed(root)?;
    let lcov = root.join(LCOV);
    writeln!(out, "xtask: running the tests under coverage")?;
    let status = Command::new(llvm_cov)
        .args(["llvm-cov", "nextest", "--workspace", "--locked", "--lcov"])
        .arg("--output-path")
        .arg(&lcov)
        .env("PATH", tools::path_with(&nextest)?)
        .env("PROPTEST_RNG_SEED", PROPTEST_RNG_SEED)
        .current_dir(root)
        .status()
        .context("could not run cargo-llvm-cov")?;
    ensure!(
        status.success(),
        "the tests failed under coverage; run `cargo xtask check` and fix them first"
    );
    let report =
        fs::read_to_string(&lcov).with_context(|| format!("could not read {}", lcov.display()))?;
    let commit = git::merge_base(root, base)?;
    let changed = git::added_lines(&git::diff(root, &commit, 0)?);
    let measured = measure(&changed, &parse_lcov(&report, root));
    write!(out, "{}", measured.describe())?;
    Ok(measured.passes())
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
}

/// Lines as comma-separated numbers and ranges, such as `3, 7-9`.
fn ranges(lines: &BTreeSet<u32>) -> String {
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for &line in lines {
        match spans.last_mut() {
            Some((_, end)) if end.checked_add(1) == Some(line) => *end = line,
            _ => spans.push((line, line)),
        }
    }
    spans
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

    use super::{Measured, measure, parse_lcov, ranges};

    const LCOV: &str = "TN:\nSF:/repo/src/a.rs\nFN:1,a\nDA:1,3\nDA:2,0\nDA:3,0\nDA:4,1\nDA:4,0\nend_of_record\nSF:/elsewhere/dep.rs\nDA:1,0\nend_of_record\nSF:/repo/src/b.rs\nDA:7,0,abc\nend_of_record\n";

    fn lines(numbers: &[u32]) -> BTreeSet<u32> {
        numbers.iter().copied().collect()
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
        assert!(
            measured(0, 0).describe().contains("(100%)"),
            "nothing to cover"
        );
        assert!(
            measured(2, 2).describe().ends_with("coverage: passed\n"),
            "passing"
        );
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
