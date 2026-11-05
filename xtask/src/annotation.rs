//! Gate findings as GitHub Actions annotations.
//!
//! Job logs live on blob storage that agent VMs cannot reach; annotations live in GitHub's API,
//! which they can (`cargo xtask pr checks`). So inside GitHub Actions each gate also prints its
//! findings as workflow commands, `::error file=<path>,line=<n>,title=<short>::<message>`,
//! escaped as GitHub's toolkit escapes them
//! (<https://github.com/actions/toolkit/blob/main/packages/core/src/command.ts>). Elsewhere the
//! output is unchanged.
//!
//! GitHub keeps at most ten annotations of each level per step, so [`commands`] prints at most
//! [`PER_LEVEL`] of each, the last of them saying how many more the log lists.

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

/// The workflow commands for `annotations` in `mode`, one per line: none in [`Mode::Plain`], and
/// at most [`PER_LEVEL`] of each level in [`Mode::Actions`], where the last one kept says how many
/// more the log lists.
pub(crate) fn commands(mode: Mode, annotations: &[Annotation]) -> String {
    let mut text = String::new();
    if mode == Mode::Plain {
        return text;
    }
    for level in [Level::Error, Level::Warning, Level::Notice] {
        let of_level: Vec<&Annotation> = annotations
            .iter()
            .filter(|annotation| annotation.level == level)
            .collect();
        let shown = if of_level.len() > PER_LEVEL {
            PER_LEVEL.saturating_sub(1)
        } else {
            of_level.len()
        };
        for annotation in of_level.iter().take(shown) {
            // Writing to a String cannot fail.
            if writeln!(text, "{annotation}").is_err() {
                break;
            }
        }
        let rest = of_level.len().saturating_sub(shown);
        if rest > 0 {
            let remainder = Annotation {
                level,
                file: None,
                lines: None,
                title: format!("{rest} more"),
                message: format!(
                    "{rest} more findings of this level; the job's log lists every one"
                ),
            };
            if writeln!(text, "{remainder}").is_err() {
                break;
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Annotation, Level, Mode, PER_LEVEL, commands};

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
        assert_eq!(
            lines[9],
            "::error title=3 more::3 more findings of this level; the job's log lists every one"
        );
        assert_eq!(lines[10], "::warning title=w::w");
        let exactly: Vec<Annotation> = many.into_iter().take(PER_LEVEL).collect();
        let ten = commands(Mode::Actions, &exactly);
        assert_eq!(ten.lines().count(), PER_LEVEL, "ten fit: {ten}");
        assert!(!ten.contains("more"), "and need no summary: {ten}");
    }
}
