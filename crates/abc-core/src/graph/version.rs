//! A graph's version: the hash of its canonical form.

use std::fmt;

use data_encoding::HEXLOWER;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use ts_rs::TS;

use super::Graph;

/// What a graph's hash covers, so no other use of the hash function can collide with it, and what
/// a change of the canonical form bumps.
const DOMAIN: &[u8] = b"abc.graph.v1\n";

/// The sha256 of a graph's canonical form: [`DOMAIN`], then the graph as compact JSON. The form
/// has no maps, no floats and no optional spacing, and its fields keep the order they are declared
/// in (`graph/mod.rs`), so the same graph hashes the same on every build and machine. Any change
/// to any step, name or edge changes the hash. A run pins one (decision 65).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
#[schemars(with = "String", extend("pattern" = "^[0-9a-f]{64}$"))]
pub struct GraphVersion([u8; 32]);

/// Text that is not a graph version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a graph version is 64 lowercase hexadecimal characters")]
pub struct InvalidGraphVersion;

impl GraphVersion {
    pub(super) fn of(graph: &Graph) -> Self {
        let mut hash = Sha256::new();
        hash.update(DOMAIN);
        // These types always serialize: no map has a non-string key and no float is NaN.
        match serde_json::to_vec(graph) {
            Ok(bytes) => hash.update(bytes),
            Err(_) => hash.update(b"unserializable"),
        }
        Self(hash.finalize().into())
    }

    /// Reads a stored hash.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The hash, for storage.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parses the hexadecimal form.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidGraphVersion`] unless it is 64 lowercase hexadecimal characters.
    pub fn parse(text: &str) -> Result<Self, InvalidGraphVersion> {
        HEXLOWER
            .decode(text.as_bytes())
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .map(Self)
            .ok_or(InvalidGraphVersion)
    }
}

impl fmt::Display for GraphVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&HEXLOWER.encode(&self.0))
    }
}

impl fmt::Debug for GraphVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GraphVersion({self})")
    }
}

impl TryFrom<String> for GraphVersion {
    type Error = InvalidGraphVersion;

    fn try_from(text: String) -> Result<Self, InvalidGraphVersion> {
        Self::parse(&text)
    }
}

impl From<GraphVersion> for String {
    fn from(version: GraphVersion) -> Self {
        version.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build::tests::{builder, chain, id, step};
    use crate::graph::{
        Access, BounceBound, Checker, EventName, GateName, LoopBound, StepKind, Text,
    };

    fn base() -> Graph {
        chain()
            .back_edge(id("c"), id("a"), LoopBound::new(2).unwrap())
            .build()
            .unwrap()
    }

    #[test]
    fn the_hash_is_pinned_across_builds_and_machines() {
        // If this changes, every pinned run's graph changed: bump DOMAIN on purpose.
        assert_eq!(
            base().version().to_string(),
            "a3ffcb19066a04a0b10a839a803a5460068e32129462cd3053bc26121414d403",
            "the canonical form's hash"
        );
    }

    #[test]
    fn the_same_graph_hashes_the_same() {
        assert_eq!(base().version(), base().version(), "deterministic");
        let rebuilt =
            serde_json::from_str::<Graph>(&serde_json::to_string(&base()).unwrap()).unwrap();
        assert_eq!(
            rebuilt.version(),
            base().version(),
            "and across serialization"
        );
    }

    /// The base graph with its middle step changed.
    fn edit(change: fn(&mut crate::graph::Step)) -> Graph {
        let mut steps: Vec<_> = base().steps().to_vec();
        change(&mut steps[1]);
        let mut b = builder();
        for step in &steps {
            b = b.step(step.clone());
        }
        b.edge(id("a"), id("b"))
            .edge(id("b"), id("c"))
            .build()
            .unwrap()
    }

    /// The base graph's steps under another name.
    fn renamed() -> Graph {
        let mut b = GraphBuilder::new(crate::graph::GraphName::parse("other").unwrap());
        for step in base().steps() {
            b = b.step(step.clone());
        }
        b.edge(id("a"), id("b"))
            .edge(id("b"), id("c"))
            .build()
            .unwrap()
    }

    /// Every way to change one step, or the graph around it.
    fn changed() -> Vec<(&'static str, Graph)> {
        vec![
            (
                "kind",
                edit(|s| {
                    s.kind = StepKind::Wait {
                        event: EventName::parse("deployed").unwrap(),
                    }
                }),
            ),
            (
                "prompt",
                edit(|s| {
                    s.kind = StepKind::Agent {
                        prompt: Text::parse("other").unwrap(),
                    }
                }),
            ),
            (
                "checker",
                edit(|s| {
                    s.checker = Checker::Gate {
                        check: GateName::parse("lint").unwrap(),
                    }
                }),
            ),
            (
                "bounces",
                edit(|s| s.max_bounces = BounceBound::new(3).unwrap()),
            ),
            (
                "access",
                edit(|s| {
                    s.access = Access {
                        untrusted_input: true,
                        ..Access::default()
                    }
                }),
            ),
            (
                "subgraph",
                edit(|s| {
                    s.kind = StepKind::Subgraph {
                        graph: base().version(),
                    }
                }),
            ),
            (
                "back bound",
                chain()
                    .back_edge(id("c"), id("a"), LoopBound::new(3).unwrap())
                    .build()
                    .unwrap(),
            ),
            (
                "back target",
                chain()
                    .back_edge(id("c"), id("b"), LoopBound::new(2).unwrap())
                    .build()
                    .unwrap(),
            ),
            ("no back edge", chain().build().unwrap()),
            (
                "one step fewer",
                builder()
                    .step(step("a"))
                    .step(step("b"))
                    .edge(id("a"), id("b"))
                    .build()
                    .unwrap(),
            ),
            (
                "order",
                builder()
                    .step(step("a"))
                    .step(step("c"))
                    .step(step("b"))
                    .edge(id("a"), id("c"))
                    .edge(id("c"), id("b"))
                    .back_edge(id("b"), id("a"), LoopBound::new(2).unwrap())
                    .build()
                    .unwrap(),
            ),
            ("name", renamed()),
        ]
    }

    use crate::graph::GraphBuilder;

    #[test]
    fn changing_anything_changes_the_hash() {
        let versions: Vec<(&str, GraphVersion)> = changed()
            .into_iter()
            .map(|(what, graph)| (what, graph.version()))
            .collect();
        for (what, version) in &versions {
            assert_ne!(
                *version,
                base().version(),
                "changing the {what} changes the hash"
            );
        }
        for (i, (a, x)) in versions.iter().enumerate() {
            for (b, y) in versions.iter().skip(i + 1) {
                assert_ne!(x, y, "{a} and {b} are different graphs");
            }
        }
    }

    #[test]
    fn a_version_round_trips_as_hex() {
        let version = base().version();
        let text = version.to_string();
        assert_eq!(text.len(), 64, "64 hex characters");
        assert_eq!(GraphVersion::parse(&text), Ok(version), "parses");
        assert_eq!(
            format!("{version:?}"),
            format!("GraphVersion({text})"),
            "debug shows it"
        );
        assert_eq!(
            GraphVersion::from_bytes(*version.as_bytes()),
            version,
            "bytes"
        );
        let json = serde_json::to_string(&version).unwrap();
        assert_eq!(json, format!("\"{text}\""), "a string on the wire");
        for bad in [
            "",
            "abc",
            &text.to_uppercase(),
            &format!("{text}0"),
            &"g".repeat(64),
        ] {
            assert_eq!(
                GraphVersion::parse(bad),
                Err(InvalidGraphVersion),
                "{bad:?} is not a version"
            );
        }
    }
}
