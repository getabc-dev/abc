//! Reads the tests in the web app's TypeScript test files (`*.test.ts`, `*.test.tsx`): Vitest's
//! under `web/src/` and Playwright's under `web/e2e/`.
//!
//! A test is a call of `test` or `it` whose first argument is its title, a string literal; a
//! `describe` call (or Playwright's `test.describe`) names the tests inside it, joined with ` > `
//! as both runners print them: `session > throws the API's error code`. A test is skipped when it
//! or a `describe` around it is `.skip`, `.todo`, `.fixme`, `.skipIf(...)`, `.runIf(...)` or
//! `x`-prefixed, or when a call such as Playwright's `test.skip(condition)` skips it, or every test
//! of its `describe` or file, at run time. `.fails` and `test.fail()` are the counterpart of
//! Rust's `#[should_panic]`. Its assertions are its calls of `expect` and `assert`, `expect.soft`
//! and the like included.
//!
//! Files are parsed with `oxc_parser`, the parser behind the Oxlint the web app's gates run, and
//! the check walks the syntax tree: strings, template literals, comments, regular expressions and
//! JSX are what the parser says they are. A file that does not parse is an error, never a file
//! without tests, and so is a title that is not a string literal or a test function's modifier
//! that is not a name: the check never counts what it cannot follow as nothing.

use anyhow::{anyhow, bail, ensure};
use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, CallExpression, Expression};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan as _, LabeledSpan, SourceType, Span};
use oxc_str::JSStr;

use super::Test;

/// The test files the check reads: `*.test.ts` and `*.test.tsx`.
pub(super) fn is_test_file(path: &str) -> bool {
    path.ends_with(".test.ts") || path.ends_with(".test.tsx")
}

/// The tests in `source`, the TypeScript test file at `path`, whose extension says whether it
/// holds JSX.
pub(super) fn tests(path: &str, source: &str) -> anyhow::Result<Vec<Test>> {
    // Test files are ES modules; only `*.test.tsx` holds JSX, where `<T>(x)` is not a type assertion.
    let source_type = SourceType::ts()
        .with_module(true)
        .with_jsx(path.ends_with(".test.tsx"));
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    // A parse that stops early always reports an error, so the tree is whole when there is none.
    if let Some(error) = parsed.diagnostics.errors().next() {
        let at = error.labels.first().map_or(0, LabeledSpan::offset);
        bail!("{path} does not parse: line {}: {error}", line(source, at));
    }
    let mut reader = Reader {
        source,
        calls: Vec::new(),
        assertions: Vec::new(),
        error: None,
    };
    reader.visit_program(&parsed.program);
    if let Some(error) = reader.error {
        return Err(error);
    }
    let Reader {
        calls, assertions, ..
    } = reader;
    let mut tests = Vec::new();
    for call in calls.iter().filter(|call| call.kind == Kind::Test) {
        let around = |kind: Kind| {
            calls
                .iter()
                .filter(move |other| other.kind == kind && other.holds(call))
        };
        let mut name = String::new();
        for describe in around(Kind::Describe) {
            name.push_str(&describe.title);
            name.push_str(" > ");
        }
        name.push_str(&call.title);
        let skipped_at_run_time = calls
            .iter()
            .any(|annotation| annotation.kind == Kind::Skip && annotation.governs(call, &calls));
        let failing_at_run_time = calls
            .iter()
            .any(|annotation| annotation.kind == Kind::Fail && annotation.governs(call, &calls));
        tests.push(Test {
            name,
            ignored: call.skipped
                || around(Kind::Describe).any(|describe| describe.skipped)
                || skipped_at_run_time,
            should_panic: call.fails || failing_at_run_time,
            assertions: assertions
                .iter()
                .filter(|&&assertion| call.arguments.contains_inclusive(assertion))
                .count(),
        });
    }
    Ok(tests)
}

/// The line, from 1, of the byte at `at` in `source`.
fn line(source: &str, at: u32) -> usize {
    source
        .get(..usize::try_from(at).unwrap_or(usize::MAX))
        .map_or(0, |before| before.matches('\n').count())
        .saturating_add(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Test,
    Describe,
    /// A call that skips the test, `describe` or file it is in at run time, such as Playwright's
    /// `test.skip(condition)`.
    Skip,
    /// A call that expects the test or tests it is in to fail, such as `test.fail()`.
    Fail,
}

/// A call of a test function, with the source range of its arguments.
#[derive(Debug)]
struct Call {
    kind: Kind,
    /// Empty for a run-time annotation.
    title: String,
    /// Whether a test or `describe` is declared skipped; an annotation's own modifier is its
    /// kind.
    skipped: bool,
    /// Whether a test or `describe` is declared to fail.
    fails: bool,
    /// From the end of the callee to the end of the call: its argument list.
    arguments: Span,
}

impl Call {
    /// Whether `other` lies within this call's arguments, or is this call.
    fn holds(&self, other: &Self) -> bool {
        self.arguments.contains_inclusive(other.arguments)
    }

    /// Whether this run-time annotation applies to `test`: it sits in that test, or in a
    /// `describe` around it, or at the top of its file, and no test or `describe` nearer to it
    /// than to `test` holds it.
    fn governs(&self, test: &Self, calls: &[Self]) -> bool {
        let innermost = calls
            .iter()
            .filter(|call| matches!(call.kind, Kind::Test | Kind::Describe) && call.holds(self))
            .min_by_key(|call| call.arguments.size());
        innermost.is_none_or(|holder| holder.holds(test))
    }
}

/// The functions whose calls declare tests and suites.
const TEST_FUNCTIONS: [&str; 7] = [
    "test",
    "it",
    "describe",
    "suite",
    "xit",
    "xtest",
    "xdescribe",
];

/// Modifiers that make a call skip what it declares.
const SKIPPING: [&str; 5] = ["skip", "todo", "fixme", "skipIf", "runIf"];

/// Modifiers whose first call takes the arguments that decide, and a second call the title.
const TWO_CALLS: [&str; 4] = ["each", "for", "skipIf", "runIf"];

/// Modifiers of `test` that are not tests: hooks, fixtures, steps and settings.
const NOT_TESTS: [&str; 12] = [
    "use",
    "extend",
    "beforeEach",
    "afterEach",
    "beforeAll",
    "afterAll",
    "step",
    "info",
    "setTimeout",
    "slow",
    "expect",
    "configure",
];

/// Walks a parsed file, collecting every test, `describe` and run-time annotation, nested ones
/// included, and where each assertion is.
struct Reader<'s> {
    source: &'s str,
    calls: Vec<Call>,
    /// Each call of `expect` or `assert`.
    assertions: Vec<Span>,
    /// The first call the check cannot follow.
    error: Option<anyhow::Error>,
}

impl<'a> Visit<'a> for Reader<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if matches!(root(&call.callee), Some("expect" | "assert")) {
            self.assertions.push(call.span);
        }
        if self.error.is_none() {
            match declaration(call) {
                Ok(Some(declared)) => self.calls.push(declared),
                Ok(None) => {}
                Err(error) => {
                    let line = line(self.source, call.span.start);
                    self.error = Some(error.context(format!("line {line}")));
                }
            }
        }
        // Go on inside the call: tests nest in describes, annotations in tests.
        walk::walk_call_expression(self, call);
    }
}

/// The name a callee reaches through its members, `expect` for `expect.soft`; `None` for a
/// callee that is not a name or members of one, such as `expect(1).toBe`.
fn root<'a>(callee: &Expression<'a>) -> Option<&'a str> {
    let callee = callee.get_inner_expression();
    if let Expression::Identifier(identifier) = callee {
        return Some(identifier.name.as_str());
    }
    root(callee.as_member_expression()?.object())
}

/// A callee as a name and the modifiers after it: `test.describe.skip` is `test`, with
/// `describe` and `skip`. A modifier is `None` when it is computed from something other than a
/// string literal. `None` for a callee that is not a name or members of one.
fn chain<'a>(callee: &Expression<'a>) -> Option<(&'a str, Vec<Option<&'a str>>)> {
    let callee = callee.get_inner_expression();
    if let Expression::Identifier(identifier) = callee {
        return Some((identifier.name.as_str(), Vec::new()));
    }
    let member = callee.as_member_expression()?;
    let (name, mut modifiers) = chain(member.object())?;
    modifiers.push(member.static_property_name().and_then(JSStr::as_str));
    Some((name, modifiers))
}

/// The test, `describe` or run-time annotation `call` declares, if any.
fn declaration(call: &CallExpression<'_>) -> anyhow::Result<Option<Call>> {
    // `test.each(table)("title", ...)`: the second call declares, with the first's modifiers.
    let (callee, second_call) =
        if let Expression::CallExpression(first) = call.callee.get_inner_expression() {
            (&first.callee, true)
        } else {
            (&call.callee, false)
        };
    let Some((function, modifiers)) = chain(callee) else {
        return Ok(None);
    };
    if !TEST_FUNCTIONS.contains(&function) {
        return Ok(None);
    }
    let Some(modifiers) = modifiers.into_iter().collect::<Option<Vec<_>>>() else {
        bail!(
            "a {function} call's modifier is computed; write it as a name so the check can tell \
             whether it skips"
        );
    };
    let has = |names: &[&str]| modifiers.iter().any(|modifier| names.contains(modifier));
    if has(&NOT_TESTS) || has(&TWO_CALLS) != second_call {
        return Ok(None);
    }
    let describe = matches!(function, "describe" | "suite" | "xdescribe") || has(&["describe"]);
    let skipped = function.starts_with('x') || has(&SKIPPING);
    let fails = has(&["fails", "fail"]);
    let arguments = Span::new(callee.span().end, call.span.end);
    let (kind, title) = if let Some(title) = title(call.arguments.first(), function)? {
        (if describe { Kind::Describe } else { Kind::Test }, title)
    } else if !describe && skipped {
        // `test.skip()`, `test.skip(condition, "why")`: a run-time annotation.
        (Kind::Skip, String::new())
    } else if !describe && fails {
        // `test.fail()`, `test.fail(condition)`.
        (Kind::Fail, String::new())
    } else {
        bail!(
            "a {function} call's first argument is not its title as a string literal; give it \
             one so the check can name it"
        );
    };
    Ok(Some(Call {
        kind,
        title,
        skipped,
        fails,
        arguments,
    }))
}

/// The title a test function's first argument gives, if it is a string literal or a template
/// literal without substitutions.
fn title(argument: Option<&Argument<'_>>, function: &str) -> anyhow::Result<Option<String>> {
    let text = if let Some(Argument::StringLiteral(literal)) = argument {
        literal.value.as_str()
    } else if let Some(Argument::TemplateLiteral(template)) = argument {
        ensure!(
            template.expressions.is_empty(),
            "a {function} title is a template literal with substitutions; give it a plain string \
             so the check can name it"
        );
        // A template literal without substitutions has one part; one whose escape is invalid has
        // no cooked value.
        template.single_quasi().and_then(JSStr::as_str)
    } else {
        return Ok(None);
    };
    // A lone surrogate is no text a test runner prints as written.
    text.map(|text| Some(text.to_owned())).ok_or_else(|| {
        anyhow!("a {function} title is not valid Unicode; write it without lone surrogates")
    })
}

#[cfg(test)]
mod tests {
    use super::{Test, is_test_file, tests};

    /// A test file without JSX, and one with.
    const TS: &str = "web/src/a.test.ts";
    const TSX: &str = "web/src/a.test.tsx";

    fn test(name: &str, assertions: usize) -> Test {
        Test {
            name: name.to_owned(),
            ignored: false,
            should_panic: false,
            assertions,
        }
    }

    fn skipped(name: &str, assertions: usize) -> Test {
        Test {
            ignored: true,
            ..test(name, assertions)
        }
    }

    #[test]
    fn reads_every_test_of_the_web_app() {
        let root = crate::repo_root().unwrap();
        let mut pending = vec![root.join("web/src"), root.join("web/e2e")];
        let mut files = 0;
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                let name = path.to_string_lossy().into_owned();
                if path.is_dir() {
                    pending.push(path);
                } else if is_test_file(&name) {
                    files += 1;
                    let found = tests(&name, &std::fs::read_to_string(&path).unwrap()).unwrap();
                    assert!(!found.is_empty(), "{name} has tests");
                    for test in found {
                        assert!(test.assertions > 0, "{name}: {test:?} asserts");
                    }
                }
            }
        }
        assert!(files > 0, "the web app has test files");
    }

    #[test]
    fn reads_only_test_files() {
        assert!(is_test_file("web/src/app.test.tsx"), "tsx");
        assert!(is_test_file("web/e2e/home.test.ts"), "ts");
        assert!(!is_test_file("web/src/app.tsx"), "not a test file");
        assert!(
            !is_test_file("web/src/test.ts"),
            "named test, but no test file"
        );
    }

    #[test]
    fn names_tests_inside_their_describes() {
        let source = r#"
import { describe, expect, test } from "vitest";

describe("session", () => {
  test("asks for JSON", async () => {
    expect(1).toBe(1);
    await expect(Promise.resolve(2)).resolves.toBe(2);
  });
  describe('errors', () => {
    it(`names the status`, () => {
      expect.soft(3).toBe(3);
      assert(true);
      assert.ok(true);
    });
  });
});

test("stands alone", () => {});
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [
                test("session > asks for JSON", 2),
                test("session > errors > names the status", 3),
                test("stands alone", 0),
            ]
        );
    }

    #[test]
    fn sees_every_way_of_skipping() {
        let source = r#"
test.skip("skipped", () => { expect(1).toBe(1); });
test.todo("to do");
it.skipIf(process.env.CI)("skipped if", () => {});
test.runIf(false)("run if", () => {});
xit("x it", () => {});
xtest("x test", () => {});
describe.skip("skipped suite", () => {
  test("inside", () => {});
});
xdescribe("x suite", () => {
  it("inside", () => {});
});
test.describe("playwright", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "chromium only");
  test("skipped by its describe", () => {});
});
test("skips itself", () => {
  test.skip();
  expect(1).toBe(1);
});
test.fixme("broken", () => {});
test("runs", () => {});
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [
                skipped("skipped", 1),
                skipped("to do", 0),
                skipped("skipped if", 0),
                skipped("run if", 0),
                skipped("x it", 0),
                skipped("x test", 0),
                skipped("skipped suite > inside", 0),
                skipped("x suite > inside", 0),
                skipped("playwright > skipped by its describe", 0),
                skipped("skips itself", 1),
                skipped("broken", 0),
                test("runs", 0),
            ]
        );
    }

    #[test]
    fn a_file_level_skip_skips_every_test() {
        let source = "test.skip(true, \"not here\");\ntest(\"a\", () => {});\n";
        assert_eq!(tests(TS, source).unwrap(), [skipped("a", 0)]);
    }

    #[test]
    fn sees_tests_expected_to_fail() {
        let source = r#"
test.fails("fails", () => {});
test("fails at run time", () => { test.fail(); });
test("passes", () => {});
"#;
        let found = tests(TS, source).unwrap();
        let failing: Vec<bool> = found.iter().map(|test| test.should_panic).collect();
        assert_eq!(failing, [true, true, false]);
    }

    #[test]
    fn reads_tables_and_ignores_hooks_steps_and_lookalikes() {
        let source = r#"
test.each([[1], [2]])("row %i", (row) => { expect(row).toBeGreaterThan(0); });
test.beforeEach(() => { expect(1).toBe(1); });
test.use({ locale: "en" });
test("steps", async () => {
  await test.step("a step", async () => { expect(1).toBe(1); });
  const pattern = /test\("not a test"\)/;
  expect(pattern.test("x")).toBe(false);
  // test("commented out", () => {});
  /* it("also commented", () => {}) */
  const text = 'it("in a string", () => {})';
  const template = `test("in a template ${text}", () => {})`;
  expect(template).toBeTruthy();
});
const ratio = 4 / 2 / 1;
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [test("row %i", 1), test("steps", 3)]
        );
    }

    #[test]
    fn reads_escapes_regular_expressions_and_nested_substitutions() {
        let source = r#"
const pattern = /[/(]test\("x"\)/gi;
function kind() { return /it\(/; }
const nested = `${ { close: "}" }.close } ${`inner ${"`"}`} ${'}'}`;
test("it's \"quoted\" \\ here", () => {
  expect(`a\`b`).toBe(nested);
});
test.each([[1]]);
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [test("it's \"quoted\" \\ here", 1)],
            "a table without its second call declares nothing"
        );
    }

    #[test]
    fn jsx_text_with_an_apostrophe_is_not_a_string() {
        let source = r#"
test("renders", () => {
  render(<p>Don't panic</p>);
  expect(page).toBeTruthy();
});
it('reads on', () => {});
"#;
        assert_eq!(
            tests(TSX, source).unwrap(),
            [test("renders", 1), test("reads on", 0)],
            "the apostrophe's line ends without closing a string"
        );
    }

    #[test]
    fn regular_expressions_and_division_are_told_apart() {
        // Each line would hide its test if a regular expression were read as division, or division
        // as a regular expression: the quotes would pair up across the test.
        let source = r#"
const quote = /"/; test("after a regex", () => {}); const again = /"/;
const slash = /[/"]/g; test("after a class", () => {}); const more = /"/;
function f() { return /"/; } test("after return", () => {}); const r = /"/;
const half = a / 2; test("between divisions", () => {}); const third = b / 3; const q = "x";
const same = (a) / b; test("after a bracket", () => {}); const s = c / d;
test   ("spaced", () => {});
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [
                test("after a regex", 0),
                test("after a class", 0),
                test("after return", 0),
                test("between divisions", 0),
                test("after a bracket", 0),
                test("spaced", 0),
            ]
        );
    }

    #[test]
    fn names_that_contain_test_or_it_are_not_tests() {
        let source = r#"
my_test("a", () => {});
$it("b", () => {});
_it("c", () => {});
x$test("d", () => {});
/** it("in a doc comment") */
test("e", () => { matcher.expect(1); expect(2); });
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [test("e", 1)],
            "a method called expect is no assertion"
        );
    }

    #[test]
    fn templates_keep_dollars_braces_and_nested_templates_in_their_substitutions() {
        let source = "test(`costs $5`, () => {});\nconst a = `${ {b: 1}.b + \"`\" }`;\nconst d = `${ `}` }`;\ntest(\"last\", () => {});\n";
        assert_eq!(
            tests(TS, source).unwrap(),
            [test("costs $5", 0), test("last", 0)]
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_naming_it_and_oxcs_message() {
        for (source, message) in [
            (
                "const a = 1;\n\n/* open",
                "line 3: Unterminated multiline comment",
            ),
            ("test(\"open\", () => {", "Expected `}` but found `EOF`"),
            ("test(\"wrong\", () => { ]);", "Unexpected token"),
            ("const a = `${ open", "Expected `}` but found `EOF`"),
            ("test.only.(\"broken modifier\");", "Unexpected token"),
        ] {
            let error = format!("{:#}", tests(TS, source).unwrap_err());
            assert!(
                error.starts_with("web/src/a.test.ts does not parse: line "),
                "{source}: {error}"
            );
            assert!(error.contains(message), "{source}: {error}");
        }
    }

    #[test]
    fn jsx_parses_only_in_tsx_files() {
        let source = "test(\"renders\", () => { render(<p>hi</p>); expect(1).toBe(1); });\n";
        assert_eq!(tests(TSX, source).unwrap(), [test("renders", 1)]);
        let error = format!("{:#}", tests(TS, source).unwrap_err());
        assert!(error.contains("does not parse"), "{error}");
    }

    #[test]
    fn reads_jsx_with_slashes_braces_and_quotes() {
        // Closing tags, self-closing tags, a regular expression and a division in expression
        // containers, quotes and test-like text in JSX text: none of them hides or invents a test.
        let source = r#"
test("renders a tree", () => {
  render(
    <div title="it('not a test')" data-ratio={4 / 2}>
      <a href="/a/b">it("in text") / test("too")</a>
      <input pattern={/^[/"]+$/.source} />
      {`${<b>{`nested ${"`"}`}</b>}`}
      Don't panic
    </div>,
  );
  expect(screen).toBeTruthy();
});
it("follows", () => { expect(1).toBe(1); });
"#;
        assert_eq!(
            tests(TSX, source).unwrap(),
            [test("renders a tree", 1), test("follows", 1)]
        );
    }

    #[test]
    fn sees_through_types_casts_and_computed_modifiers() {
        let source = r#"
type Row = { n: number };
const rows: Row[] = [{ n: 1 }];
(test.skip as typeof test)("cast", () => {});
test["skip"]("computed", () => {});
test<Row>("typed", (row: Row) => { expect(row!.n satisfies number).toBe(1); });
"#;
        assert_eq!(
            tests(TS, source).unwrap(),
            [skipped("cast", 0), skipped("computed", 0), test("typed", 1)]
        );
        let error = format!(
            "{:#}",
            tests(TS, "test[mode](\"a\", () => {});").unwrap_err()
        );
        assert!(
            error.contains("line 1: a test call's modifier is computed"),
            "{error}"
        );
    }

    #[test]
    fn the_first_call_it_cannot_follow_is_the_one_reported() {
        let source = "test(first, () => {});\n\ntest(`second ${n}`, () => {});\n";
        let error = format!("{:#}", tests(TS, source).unwrap_err());
        assert!(error.starts_with("line 1: "), "{error}");
        assert!(error.contains("is not its title"), "{error}");
    }

    #[test]
    fn refuses_titles_it_cannot_name() {
        for (source, problem) in [
            (
                "test(`row ${n}`, () => {});",
                "template literal with substitutions",
            ),
            ("test(title, () => {});", "is not its title"),
            ("describe(name, () => {});", "is not its title"),
            (
                "\n\ntest(42, () => {});",
                "line 3: a test call's first argument",
            ),
            ("it(\"\\uD800\", () => {});", "not valid Unicode"),
        ] {
            let error = format!("{:#}", tests(TS, source).unwrap_err());
            assert!(error.contains(problem), "{source}: {error}");
        }
    }
}
