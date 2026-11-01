//! The test inventory of a Rust file: every `#[test]` function, including those inside
//! `proptest!`, with its markers and how many assertion macros it calls.

use anyhow::Context;
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::visit::{self, Visit};

use super::Test;

/// The tests in a Rust source file.
pub(super) fn tests(source: &str) -> anyhow::Result<Vec<Test>> {
    let file = syn::parse_file(source).context("the file is not valid Rust")?;
    let mut finder = Finder {
        module: Vec::new(),
        tests: Vec::new(),
    };
    finder.visit_file(&file);
    Ok(finder.tests)
}

struct Finder {
    module: Vec<String>,
    tests: Vec<Test>,
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
        let markers: Vec<String> = item.attrs.iter().filter_map(attribute_name).collect();
        if markers.iter().any(|name| name == "test") {
            let mut assertions = Assertions(0);
            assertions.visit_block(&item.block);
            self.tests.push(Test {
                name: self.qualified(&item.sig.ident.to_string()),
                ignored: markers.iter().any(|name| name == "ignore"),
                should_panic: markers.iter().any(|name| name == "should_panic"),
                assertions: assertions.0,
            });
        }
        visit::visit_item_fn(self, item);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if macro_name(&item.mac.path).as_deref() == Some("proptest") {
            let tests = macro_tests(item.mac.tokens.clone());
            for (name, test) in tests {
                self.tests.push(Test {
                    name: self.qualified(&name),
                    ..test
                });
            }
        }
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

/// The `#[test]` functions inside a `proptest!` block, which uses `in` patterns syn cannot parse
/// as functions, read from its tokens.
fn macro_tests(tokens: TokenStream) -> Vec<(String, Test)> {
    let mut tests = Vec::new();
    let mut markers: Vec<String> = Vec::new();
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                if let Some(TokenTree::Group(group)) = tokens.peek()
                    && group.delimiter() == Delimiter::Bracket
                {
                    if let Some(TokenTree::Ident(name)) = group.stream().into_iter().next() {
                        markers.push(name.to_string());
                    }
                    tokens.next();
                }
            }
            TokenTree::Ident(keyword) if keyword == "fn" => {
                let Some(TokenTree::Ident(name)) = tokens.next() else {
                    continue;
                };
                let name = name.to_string();
                let body = tokens.find_map(|candidate| match candidate {
                    TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                        Some(group.stream())
                    }
                    TokenTree::Group(_)
                    | TokenTree::Ident(_)
                    | TokenTree::Punct(_)
                    | TokenTree::Literal(_) => None,
                });
                if markers.iter().any(|marker| marker == "test") {
                    tests.push((
                        name.clone(),
                        Test {
                            name,
                            ignored: markers.iter().any(|marker| marker == "ignore"),
                            should_panic: markers.iter().any(|marker| marker == "should_panic"),
                            assertions: body.map_or(0, count_assertions),
                        },
                    ));
                }
                markers.clear();
            }
            TokenTree::Punct(_)
            | TokenTree::Ident(_)
            | TokenTree::Group(_)
            | TokenTree::Literal(_) => {}
        }
    }
    tests
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
            "proptest! bodies are read from their tokens"
        );
    }

    #[test]
    fn rejects_files_that_are_not_rust() {
        assert!(tests("fn (").is_err(), "a parse error is reported");
    }
}
