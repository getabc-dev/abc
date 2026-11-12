//! Graphs: what abc runs, as typed values that cannot be invalid (decision 65).
//!
//! A [`Graph`] is a list of steps in execution order. Every step is a **maker** (its [`StepKind`])
//! judged by a **checker** ([`Checker`]), which passes it, loops, bounces it back to the maker
//! (at most [`BounceBound`] times) or escalates to a person. After a step passes the run moves to
//! the next, and a step may have one **back edge** to an earlier step or itself, taken at most
//! [`LoopBound`] times in a run. This is the shape `spec/runner/` models: `StepDef` is a [`Step`],
//! `maxBounces` is [`Step::max_bounces`] and `loop` is [`Step::back_edge`].
//!
//! There is no way to hold a graph that breaks a rule. [`GraphBuilder::build`] is the only way to
//! make one, and deserializing one runs the same build, so a graph read from JSON is as valid as
//! one built in code. What `build` refuses is in [`GraphError`]. [`GraphVersion`] is the hash of
//! a graph's canonical form, which a run pins.

mod build;
mod version;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use build::{GraphBuilder, GraphError};
pub use version::{GraphVersion, InvalidGraphVersion};

use crate::identity::InvalidValue;

macro_rules! slug {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS,
            JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[ts(type = "string")]
        #[schemars(with = "String")]
        pub struct $name(String);

        impl $name {
            /// Parses 1 to 48 characters of `a-z`, `0-9`, `-` and `_`, starting with a letter.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidValue`] for anything else.
            pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
                let mut chars = raw.chars();
                let valid = raw.len() <= 48
                    && chars.next().is_some_and(|first| first.is_ascii_lowercase())
                    && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
                if valid {
                    Ok(Self(raw.to_owned()))
                } else {
                    Err(InvalidValue {
                        field: $field,
                        rule: "1 to 48 characters of a-z, 0-9, - and _, starting with a letter",
                    })
                }
            }

            /// The name as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidValue;

            fn try_from(raw: String) -> Result<Self, InvalidValue> {
                Self::parse(&raw)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> Self {
                name.0
            }
        }
    };
}

slug!(
    /// A step's name, unique in its graph.
    StepId,
    "step id"
);
slug!(
    /// A graph's name. Two graphs of one name differ by their [`GraphVersion`].
    GraphName,
    "graph name"
);
slug!(
    /// The named check a gate runs.
    GateName,
    "gate name"
);
slug!(
    /// The named event a wait step waits for.
    EventName,
    "event name"
);

/// Instructions or a command line: 1 to 16384 bytes, with no control characters but newline and
/// tab.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
#[schemars(with = "String")]
pub struct Text(String);

impl Text {
    const MAX_LEN: usize = 16 * 1024;

    /// Parses instructions.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for empty or over-long text, or one with control characters.
    pub fn parse(raw: &str) -> Result<Self, InvalidValue> {
        let valid = !raw.trim().is_empty()
            && raw.len() <= Self::MAX_LEN
            && raw
                .chars()
                .all(|c| !c.is_control() || c == '\n' || c == '\t');
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidValue {
                field: "text",
                rule: "1 to 16384 bytes, with no control characters but newline and tab",
            })
        }
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Text {
    type Error = InvalidValue;

    fn try_from(raw: String) -> Result<Self, InvalidValue> {
        Self::parse(&raw)
    }
}

impl From<Text> for String {
    fn from(text: Text) -> Self {
        text.0
    }
}

/// How many times a checker may bounce a step's work back to its maker before the step escalates
/// to a person: 0 to 100 (spec `maxBounces`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(try_from = "u32", into = "u32")]
#[ts(type = "number")]
#[schemars(with = "u32", extend("minimum" = 0, "maximum" = 100))]
pub struct BounceBound(u32);

/// How many times a run may take a back edge: 1 to 100, so a cycle always ends (spec `bound`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(try_from = "u32", into = "u32")]
#[ts(type = "number")]
#[schemars(with = "u32", extend("minimum" = 1, "maximum" = 100))]
pub struct LoopBound(u32);

/// The largest bound of either kind.
const MAX_BOUND: u32 = 100;

macro_rules! bound {
    ($name:ident, $min:expr, $field:literal, $rule:literal) => {
        impl $name {
            /// Checks the range.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidValue`] outside the range.
            pub const fn new(count: u32) -> Result<Self, InvalidValue> {
                const MIN: u32 = $min;
                if count >= MIN && count <= MAX_BOUND {
                    Ok(Self(count))
                } else {
                    Err(InvalidValue {
                        field: $field,
                        rule: $rule,
                    })
                }
            }

            /// The count.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl TryFrom<u32> for $name {
            type Error = InvalidValue;

            fn try_from(count: u32) -> Result<Self, InvalidValue> {
                Self::new(count)
            }
        }

        impl From<$name> for u32 {
            fn from(bound: $name) -> Self {
                bound.0
            }
        }
    };
}

bound!(BounceBound, 0, "bounce bound", "0 to 100");
bound!(LoopBound, 1, "loop bound", "1 to 100");

/// The work a step's maker does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    /// An agent works from instructions.
    Agent {
        /// What to do.
        prompt: Text,
    },
    /// A command runs in the sandbox.
    Code {
        /// The command line.
        command: Text,
    },
    /// A named gate (a deterministic check, such as the tests) runs.
    Gate {
        /// Which one.
        check: GateName,
    },
    /// The run waits for an outside event, such as a webhook or a deploy finishing.
    Wait {
        /// Which one.
        event: EventName,
    },
    /// Another graph runs as this step, pinned by its version.
    Subgraph {
        /// The graph, by version.
        graph: GraphVersion,
    },
}

/// Who judges a step's work, with fresh eyes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Checker {
    /// A fresh agent reviews the work.
    Agent {
        /// What to look for.
        prompt: Text,
    },
    /// A command decides.
    Code {
        /// The command line.
        command: Text,
    },
    /// A named gate decides.
    Gate {
        /// Which one.
        check: GateName,
    },
}

/// What a step may touch. A step with all three at once is exposed to prompt injection that can
/// act and leak: the Rule of Two refuses it, so every step has at most two.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS, JsonSchema,
)]
pub struct Access {
    /// It reads input an outsider can write (web pages, issues, other people's code).
    pub untrusted_input: bool,
    /// It reads private data (secrets, customer data, private repositories).
    pub private_data: bool,
    /// It acts outside its change: network writes, other repositories, deployments.
    pub outside_actions: bool,
}

impl Access {
    /// Whether all three are held at once.
    #[must_use]
    pub const fn breaks_rule_of_two(self) -> bool {
        self.untrusted_input && self.private_data && self.outside_actions
    }
}

/// A back edge: when the step passes and the checker asks to loop, the run goes back to `target`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
pub struct BackEdge {
    /// An earlier step, or this one.
    pub target: StepId,
    /// How many times a run may take it.
    pub bound: LoopBound,
}

/// One step: a maker's work, its checker, and what it may touch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
pub struct Step {
    /// Its name.
    pub id: StepId,
    /// The maker's work.
    pub kind: StepKind,
    /// Who judges it.
    pub checker: Checker,
    /// How often the checker may send the work back before a person decides.
    pub max_bounces: BounceBound,
    /// What it may touch.
    pub access: Access,
    /// Where it may loop back to, if anywhere.
    pub back_edge: Option<BackEdge>,
}

/// A valid graph: steps in execution order. Only [`GraphBuilder::build`] and deserializing make
/// one, and both run the same checks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(try_from = "GraphWire")]
pub struct Graph {
    name: GraphName,
    steps: Vec<Step>,
}

/// A graph as written down, before its checks.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(rename = "Graph")]
struct GraphWire {
    name: GraphName,
    steps: Vec<Step>,
}

impl TryFrom<GraphWire> for Graph {
    type Error = GraphError;

    fn try_from(wire: GraphWire) -> Result<Self, GraphError> {
        let mut builder = GraphBuilder::new(wire.name);
        let mut previous: Option<StepId> = None;
        for step in wire.steps {
            if let Some(from) = previous.take() {
                builder = builder.edge(from, step.id.clone());
            }
            previous = Some(step.id.clone());
            builder = builder.step(step);
        }
        builder.build()
    }
}

/// A forward or back edge of a graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge<'a> {
    /// Where it leaves.
    pub from: &'a StepId,
    /// Where it arrives.
    pub to: &'a StepId,
    /// Whether it is the step after, or a bounded way back.
    pub kind: EdgeKind,
}

/// The two kinds of edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// To the next step.
    Forward,
    /// Back, at most this many times in a run.
    Back(LoopBound),
}

impl Graph {
    /// Its name.
    #[must_use]
    pub const fn name(&self) -> &GraphName {
        &self.name
    }

    /// The steps, in execution order. Never empty.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Every edge: the forward edges in execution order, then the back edges in step order.
    pub fn edges(&self) -> impl Iterator<Item = Edge<'_>> {
        let forward = self
            .steps
            .iter()
            .zip(self.steps.iter().skip(1))
            .map(|(from, to)| Edge {
                from: &from.id,
                to: &to.id,
                kind: EdgeKind::Forward,
            });
        let back = self.steps.iter().filter_map(|step| {
            step.back_edge.as_ref().map(|edge| Edge {
                from: &step.id,
                to: &edge.target,
                kind: EdgeKind::Back(edge.bound),
            })
        });
        forward.chain(back)
    }

    /// The version a run pins: the hash of this graph's canonical form.
    #[must_use]
    pub fn version(&self) -> GraphVersion {
        GraphVersion::of(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_lowercase_slugs() {
        for good in ["a", "build-1", "check_it", &"a".repeat(48)] {
            assert_eq!(
                StepId::parse(good).unwrap().as_str(),
                good,
                "{good} is a name"
            );
        }
        for bad in ["", "A", "1a", "-a", "a b", "é", &"a".repeat(49)] {
            assert!(StepId::parse(bad).is_err(), "{bad:?} is refused");
        }
        assert_eq!(
            serde_json::from_str::<GateName>("\"tests\"")
                .unwrap()
                .as_str(),
            "tests",
            "a name reads from JSON"
        );
        assert!(
            serde_json::from_str::<EventName>("\"No\"").is_err(),
            "and is checked there"
        );
    }

    #[test]
    fn text_is_bounded_and_printable() {
        assert_eq!(
            Text::parse("do it\n\tnow").unwrap().as_str(),
            "do it\n\tnow",
            "newline and tab are text, kept as written"
        );
        assert!(
            Text::parse(&"x".repeat(16 * 1024)).is_ok(),
            "16 KiB is the longest allowed"
        );
        for bad in ["", "  \n", "a\u{0}b", &"x".repeat(16 * 1024 + 1)] {
            assert!(Text::parse(bad).is_err(), "{:?} is refused", bad.len());
        }
        assert_eq!(
            String::from(Text::parse("x").unwrap()),
            "x",
            "text converts back"
        );
    }

    #[test]
    fn bounds_keep_their_ranges() {
        assert_eq!(
            BounceBound::new(0).unwrap().get(),
            0,
            "no bounces is allowed"
        );
        assert_eq!(BounceBound::new(100).unwrap().get(), 100, "the top");
        assert!(BounceBound::new(101).is_err(), "over the top");
        assert!(
            LoopBound::new(0).is_err(),
            "a back edge taken zero times is no edge"
        );
        assert_eq!(u32::from(LoopBound::try_from(7).unwrap()), 7, "round trip");
        assert!(
            serde_json::from_str::<LoopBound>("0").is_err(),
            "checked from JSON"
        );
    }

    #[test]
    fn only_all_three_accesses_break_the_rule() {
        let access = |a, b, c| Access {
            untrusted_input: a,
            private_data: b,
            outside_actions: c,
        };
        assert!(!access(false, false, false).breaks_rule_of_two(), "none");
        for two in [
            (true, true, false),
            (true, false, true),
            (false, true, true),
        ] {
            assert!(
                !access(two.0, two.1, two.2).breaks_rule_of_two(),
                "{two:?} is two"
            );
        }
        assert!(access(true, true, true).breaks_rule_of_two(), "three");
    }
}
