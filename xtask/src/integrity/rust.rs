//! The test inventory of a Rust file: every `#[test]` function, including those inside
//! `proptest!`, with its markers and how many assertion macros it calls.

use anyhow::{Context, anyhow};
use proc_macro2::{TokenStream, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::visit::{self, Visit};

use super::Test;

/// The tests in a Rust source file.
pub(super) fn tests(source: &str) -> anyhow::Result<Vec<Test>> {
    let file = syn::parse_file(source).context("the file is not valid Rust")?;
    let mut finder = Finder {
        module: Vec::new(),
        tests: Vec::new(),
        error: None,
    };
    finder.visit_file(&file);
    match finder.error {
        Some(error) => Err(error),
        None => Ok(finder.tests),
    }
}

struct Finder {
    module: Vec<String>,
    tests: Vec<Test>,
    /// The first `proptest!` block that could not be read.
    error: Option<anyhow::Error>,
}

impl Finder {
    fn qualified(&self, name: &str) -> String {
        self.module
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(name))
            .collect::<Vec<_>>()
            .join("::")
    }
}

impl<'ast> Visit<'ast> for Finder {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.module.push(item.ident.to_string());
        visit::visit_item_mod(self, item);
        self.module.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if let Some(test) = test(&item.attrs, &item.sig.ident, &item.block) {
            self.tests.push(Test {
                name: self.qualified(&test.name),
                ..test
            });
        }
        visit::visit_item_fn(self, item);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if macro_name(&item.mac.path).as_deref() != Some("proptest") {
            return;
        }
        match item.mac.parse_body::<PropTests>() {
            Ok(PropTests(functions)) => {
                for function in functions {
                    if let Some(test) = test(&function.attrs, &function.name, &function.body) {
                        self.tests.push(Test {
                            name: self.qualified(&test.name),
                            ..test
                        });
                    }
                }
            }
            Err(error) => {
                let place = if self.module.is_empty() {
                    "the file".to_owned()
                } else {
                    self.module.join("::")
                };
                self.error.get_or_insert_with(|| {
                    anyhow!("could not read the proptest! block in {place}: {error}")
                });
            }
        }
    }
}

/// The test a function is, if its attributes mark it as one.
fn test(attrs: &[syn::Attribute], name: &syn::Ident, body: &syn::Block) -> Option<Test> {
    let markers: Vec<String> = attrs.iter().filter_map(attribute_name).collect();
    if !markers.iter().any(|marker| marker == "test") {
        return None;
    }
    let mut assertions = Assertions(0);
    assertions.visit_block(body);
    Some(Test {
        name: name.to_string(),
        ignored: markers.iter().any(|marker| marker == "ignore"),
        should_panic: markers.iter().any(|marker| marker == "should_panic"),
        assertions: assertions.0,
    })
}

/// The functions of a `proptest!` block: inner attributes such as `#![proptest_config(..)]`,
/// then functions whose parameters bind strategies with `in`, which are not Rust patterns and
/// are skipped.
struct PropTests(Vec<PropFunction>);

struct PropFunction {
    attrs: Vec<syn::Attribute>,
    name: syn::Ident,
    body: syn::Block,
}

impl Parse for PropTests {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        input.call(syn::Attribute::parse_inner)?;
        let mut functions = Vec::new();
        while !input.is_empty() {
            let attrs = input.call(syn::Attribute::parse_outer)?;
            input.parse::<syn::Token![fn]>()?;
            let name = input.parse()?;
            let parameters;
            syn::parenthesized!(parameters in input);
            parameters.parse::<TokenStream>()?;
            if input.peek(syn::Token![->]) {
                input.parse::<syn::Token![->]>()?;
                input.parse::<syn::Type>()?;
            }
            functions.push(PropFunction {
                attrs,
                name,
                body: input.parse()?,
            });
        }
        Ok(Self(functions))
    }
}

/// The last segment of an attribute's path, such as `test` for `#[tokio::test]`.
fn attribute_name(attribute: &syn::Attribute) -> Option<String> {
    attribute
        .path()
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn macro_name(path: &syn::Path) -> Option<String> {
    path.segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn is_assertion(name: &str) -> bool {
    name.contains("assert")
}

/// Counts assertion macros, such as `assert_eq!` or `insta::assert_snapshot!`.
struct Assertions(usize);

impl<'ast> Visit<'ast> for Assertions {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if macro_name(&mac.path).is_some_and(|name| is_assertion(&name)) {
            self.0 = self.0.saturating_add(1);
        }
        self.0 = self.0.saturating_add(count_assertions(mac.tokens.clone()));
    }
}

/// Assertion macro calls inside raw tokens, which syn leaves unparsed: an identifier naming an
/// assertion followed by `!`.
fn count_assertions(tokens: TokenStream) -> usize {
    let mut count: usize = 0;
    let mut previous_assertion = false;
    for token in tokens {
        match token {
            TokenTree::Ident(ident) => previous_assertion = is_assertion(&ident.to_string()),
            TokenTree::Punct(punct) => {
                if previous_assertion && punct.as_char() == '!' {
                    count = count.saturating_add(1);
                }
                previous_assertion = false;
            }
            TokenTree::Group(group) => {
                count = count.saturating_add(count_assertions(group.stream()));
                previous_assertion = false;
            }
            TokenTree::Literal(_) => previous_assertion = false,
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::tests;
    use crate::integrity::Test;

    fn test(name: &str, ignored: bool, should_panic: bool, assertions: usize) -> Test {
        Test {
            name: name.to_owned(),
            ignored,
            should_panic,
            assertions,
        }
    }

    #[test]
    fn finds_tests_with_their_markers_and_assertions() {
        let source = r#"
            fn helper() { assert!(true); }

            #[cfg(test)]
            mod tests {
                #[test]
                fn plain() {
                    assert_eq!(1, 1);
                    for x in [1] { assert!(x > 0, "{x}"); }
                    insta::assert_snapshot!("x", @"x");
                }

                #[test]
                #[ignore = "slow"]
                fn ignored() {}

                #[tokio::test]
                #[should_panic(expected = "boom")]
                async fn panics() { panic!("boom"); }

                mod nested {
                    #[test]
                    fn deep() { let v = vec![assert_ne!(1, 2)]; drop(v); }
                }
            }
        "#;
        assert_eq!(
            tests(source).unwrap(),
            [
                test("tests::plain", false, false, 3),
                test("tests::ignored", true, false, 0),
                test("tests::panics", false, true, 0),
                test("tests::nested::deep", false, false, 1),
            ],
            "helpers are not tests; macros inside macros count"
        );
    }

    #[test]
    fn finds_tests_inside_proptest() {
        let source = r"
            mod tests {
                proptest! {
                    #![proptest_config(config())]

                    #[test]
                    fn round_trips(x in any::<u8>()) {
                        prop_assert_eq!(x, x);
                        prop_assert!(true);
                    }

                    #[test]
                    #[ignore]
                    fn skipped(y in 0..3) { let _ = y; }
                }
            }
        ";
        assert_eq!(
            tests(source).unwrap(),
            [
                test("tests::round_trips", false, false, 2),
                test("tests::skipped", true, false, 0),
            ],
            "proptest! functions are parsed; their strategies are skipped"
        );
    }

    #[test]
    fn rejects_a_proptest_block_it_cannot_read() {
        let error = tests("mod tests { proptest! { #[test] fn broken } }")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("could not read the proptest! block in tests"),
            "{error}"
        );
    }

    #[test]
    fn counts_only_assertion_macros_inside_other_macros() {
        let source = r#"
            #[test]
            fn t() {
                let asserted = vec![!false, 1 + 1 == 2];
                println!("{}", asserted.len());
                let calls = vec![assert!(true), debug_assert_eq!(1, 1)];
                drop(calls);
            }
        "#;
        assert_eq!(
            tests(source).unwrap(),
            [test("t", false, false, 2)],
            "a name containing assert, or a ! after another word, is no assertion"
        );
    }

    #[test]
    fn rejects_files_that_are_not_rust() {
        assert!(tests("fn (").is_err(), "a parse error is reported");
    }
}
