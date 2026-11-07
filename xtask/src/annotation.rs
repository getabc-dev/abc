//! Gate findings as GitHub Actions annotations.
//!
//! Job logs live on blob storage that agent VMs cannot reach; annotations live in GitHub's API,
//! which they can (`cargo xtask pr checks`). So inside GitHub Actions each gate also prints its
//! findings as workflow commands, `::error file=<path>,line=<n>,title=<short>::<message>`,
//! escaped as GitHub's toolkit escapes them
//! (<https://github.com/actions/toolkit/blob/main/packages/core/src/command.ts>). Elsewhere the
//! output is unchanged.
//!
//! GitHub keeps at most ten annotations of each level per step, whichever part of the step printed
//! them, so every source in one step prints through one [`Step`], which prints at most
//! [`PER_LEVEL`] of each level, the last of them saying how many more the log lists.

use std::ffi::OsString;
use std::fmt::{self, Write as _};

/// How many annotations of one level GitHub keeps for a step.
pub(crate) const PER_LEVEL: usize = 10;

/// Where a gate's output goes, which decides whether it annotates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A terminal or a log: the findings as text only.
    Plain,
    /// A GitHub Actions step: the findings as text, then as workflow commands.
    Actions,
}

impl Mode {
    /// [`Mode::Actions`] when `GITHUB_ACTIONS` is `true`, as GitHub Actions sets it.
    pub(crate) fn from_env(env: &impl Fn(&str) -> Option<OsString>) -> Self {
        if env("GITHUB_ACTIONS").is_some_and(|value| value == "true") {
            Self::Actions
        } else {
            Self::Plain
        }
    }
}

/// How serious an annotation is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Error,
    Warning,
    Notice,
}

impl Level {
    const fn command(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Notice => "notice",
        }
    }
}

/// One finding, as GitHub shows it on the pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Annotation {
    pub(crate) level: Level,
    /// The file, relative to the repository root.
    pub(crate) file: Option<String>,
    /// The first line and the last, in the file.
    pub(crate) lines: Option<(u64, u64)>,
    pub(crate) title: String,
    pub(crate) message: String,
}

impl Annotation {
    /// An error annotation on `file`.
    pub(crate) fn error(file: &str, title: &str, message: String) -> Self {
        Self {
            level: Level::Error,
            file: Some(file.to_owned()),
            lines: None,
            title: title.to_owned(),
            message,
        }
    }

    /// The same annotation on lines `first` to `last`.
    pub(crate) const fn at(mut self, first: u64, last: u64) -> Self {
        self.lines = Some((first, last));
        self
    }
}

impl fmt::Display for Annotation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut properties = Vec::new();
        if let Some(file) = &self.file {
            properties.push(format!("file={}", property(file)));
        }
        if let Some((first, last)) = self.lines {
            properties.push(format!("line={first}"));
            if last != first {
                properties.push(format!("endLine={last}"));
            }
        }
        properties.push(format!("title={}", property(&self.title)));
        write!(
            f,
            "::{} {}::{}",
            self.level.command(),
            properties.join(","),
            data(&self.message)
        )
    }
}

/// A command's message, escaped as GitHub's toolkit does (`escapeData`).
fn data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// A command's property value, escaped as GitHub's toolkit does (`escapeProperty`).
fn property(text: &str) -> String {
    data(text).replace(':', "%3A").replace(',', "%2C")
}

/// One GitHub Actions step's annotations, across every source that prints them (each workspace's
/// failing tests, then the lines coverage finds untested, say). It prints each annotation as it
/// comes, up to [`PER_LEVEL`] less one of each level, and holds the rest; [`Step::finish`] then
/// prints the one held, or a single note saying how many more the log lists. In [`Mode::Plain`] it
/// prints nothing.
#[derive(Debug)]
pub(crate) struct Step {
    mode: Mode,
    errors: Tally,
    warnings: Tally,
    notices: Tally,
}

/// The levels, in the order [`Step::finish`] closes them.
const LEVELS: [Level; 3] = [Level::Error, Level::Warning, Level::Notice];

/// What a [`Step`] did with one level's annotations.
#[derive(Debug, Default)]
struct Tally {
    /// How many it printed.
    shown: usize,
    /// The first it held back, printed by [`Step::finish`] when it is the only one.
    first: Option<Annotation>,
    /// How many it held back.
    held: usize,
}

impl Step {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
            mode,
            errors: Tally::default(),
            warnings: Tally::default(),
            notices: Tally::default(),
        }
    }

    const fn tally(&mut self, level: Level) -> &mut Tally {
        match level {
            Level::Error => &mut self.errors,
            Level::Warning => &mut self.warnings,
            Level::Notice => &mut self.notices,
        }
    }

    /// The workflow commands for `annotations`, one per line, as many as the step still prints.
    pub(crate) fn emit(&mut self, annotations: &[Annotation]) -> String {
        let mut text = String::new();
        if self.mode == Mode::Plain {
            return text;
        }
        for annotation in annotations {
            let held = self.tally(annotation.level);
            if held.shown < PER_LEVEL.saturating_sub(1) {
                held.shown = held.shown.saturating_add(1);
                // Writing to a String cannot fail.
                if writeln!(text, "{annotation}").is_err() {
                    break;
                }
            } else {
                held.held = held.held.saturating_add(1);
                if held.first.is_none() {
                    held.first = Some(annotation.clone());
                }
            }
        }
        text
    }

    /// The step's last commands: for each level, the one annotation held back, or a note saying
    /// how many more the log lists. Afterwards nothing is held.
    pub(crate) fn finish(&mut self) -> String {
        let mut text = String::new();
        for level in LEVELS {
            let held = std::mem::take(self.tally(level));
            let last = match (held.held, held.first) {
                (1, Some(only)) => only,
                (0, _) | (_, None) => continue,
                (rest, Some(_)) => Annotation {
                    level,
                    file: None,
                    lines: None,
                    title: format!("{rest} more"),
                    message: format!(
                        "{rest} more findings of this level; the job's log lists every one"
                    ),
                },
            };
            if writeln!(text, "{last}").is_err() {
                break;
            }
        }
        text
    }
}

/// The workflow commands for `annotations` in `mode` when they are the step's only ones: a
/// [`Step`] that prints them all, then finishes.
pub(crate) fn commands(mode: Mode, annotations: &[Annotation]) -> String {
    let mut step = Step::new(mode);
    let mut text = step.emit(annotations);
    text.push_str(&step.finish());
    text
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Annotation, Level, Mode, PER_LEVEL, Step, commands};

    #[test]
    fn annotates_only_inside_github_actions() {
        let actions = |name: &str| (name == "GITHUB_ACTIONS").then(|| OsString::from("true"));
        assert_eq!(Mode::from_env(&actions), Mode::Actions);
        let other = |name: &str| (name == "GITHUB_ACTIONS").then(|| OsString::from("1"));
        assert_eq!(Mode::from_env(&other), Mode::Plain);
        assert_eq!(Mode::from_env(&|_: &str| None), Mode::Plain);
        let finding = Annotation::error("a.rs", "t", "m".to_owned());
        assert_eq!(commands(Mode::Plain, std::slice::from_ref(&finding)), "");
        assert_eq!(
            commands(Mode::Actions, &[finding]),
            "::error file=a.rs,title=t::m\n"
        );
    }

    #[test]
    fn escapes_as_githubs_toolkit_does() {
        let annotation = Annotation::error(
            "dir,x/a:b.rs",
            "50% done, a: b",
            "line one\r\nline two: 100%, done".to_owned(),
        )
        .at(3, 7);
        assert_eq!(
            annotation.to_string(),
            "::error file=dir%2Cx/a%3Ab.rs,line=3,endLine=7,title=50%25 done%2C a%3A b::line \
             one%0D%0Aline two: 100%25, done"
        );
        let single = Annotation {
            level: Level::Notice,
            file: None,
            lines: Some((4, 4)),
            title: "t".to_owned(),
            message: "m".to_owned(),
        };
        assert_eq!(single.to_string(), "::notice line=4,title=t::m");
        let warning = Annotation {
            level: Level::Warning,
            ..single
        };
        assert_eq!(warning.to_string(), "::warning line=4,title=t::m");
    }

    #[test]
    fn keeps_what_github_keeps_and_counts_the_rest() {
        let many: Vec<Annotation> = (1..=12)
            .map(|n| Annotation::error("a.rs", "t", format!("m{n}")).at(n, n))
            .chain(std::iter::once(Annotation {
                level: Level::Warning,
                file: None,
                lines: None,
                title: "w".to_owned(),
                message: "w".to_owned(),
            }))
            .collect();
        let text = commands(Mode::Actions, &many);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), PER_LEVEL + 1, "{text}");
        assert_eq!(lines[0], "::error file=a.rs,line=1,title=t::m1");
        assert_eq!(lines[8], "::error file=a.rs,line=9,title=t::m9");
        assert_eq!(lines[9], "::warning title=w::w");
        assert_eq!(
            lines[10],
            "::error title=3 more::3 more findings of this level; the job's log lists every one"
        );
        let exactly: Vec<Annotation> = many.into_iter().take(PER_LEVEL).collect();
        let ten = commands(Mode::Actions, &exactly);
        assert_eq!(ten.lines().count(), PER_LEVEL, "ten fit: {ten}");
        assert!(!ten.contains("more"), "and need no summary: {ten}");
    }

    /// `check` annotated each failing workspace's tests on its own, so one step could print ten
    /// errors for each, and GitHub dropped all past the tenth without a word.
    #[test]
    fn one_step_keeps_ten_across_its_sources() {
        let failing = |workspace: &str, count: u64| -> Vec<Annotation> {
            (1..=count)
                .map(|n| Annotation::error(&format!("{workspace}/a.rs"), "t", format!("m{n}")))
                .collect()
        };
        let mut step = Step::new(Mode::Actions);
        let mut text = step.emit(&failing("main", 10));
        text.push_str(&step.emit(&failing("xtask", 3)));
        text.push_str(&step.finish());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), PER_LEVEL, "ten in all: {text}");
        assert_eq!(
            lines.last().copied(),
            Some(
                "::error title=4 more::4 more findings of this level; the job's log lists every one"
            ),
            "one note for both workspaces: {text}"
        );
        assert_eq!(step.finish(), "", "a finished step holds nothing");

        let mut exactly = Step::new(Mode::Actions);
        let mut ten = exactly.emit(&failing("main", 9));
        ten.push_str(&exactly.emit(&failing("xtask", 1)));
        ten.push_str(&exactly.finish());
        assert_eq!(ten.lines().count(), PER_LEVEL, "{ten}");
        assert!(
            ten.ends_with("::error file=xtask/a.rs,title=t::m1\n"),
            "the tenth is kept: {ten}"
        );

        let mut plain = Step::new(Mode::Plain);
        assert_eq!(plain.emit(&failing("main", 12)), "");
        assert_eq!(plain.finish(), "");
    }
}
