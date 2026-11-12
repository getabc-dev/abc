//! The graph builder: the only way to make a [`Graph`], and where every rule is checked.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use super::{BackEdge, Graph, GraphName, LoopBound, Step, StepId};

/// Why a graph cannot be built. Each names the step it is about.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    /// A graph with no step.
    #[error("the graph has no steps")]
    Empty,
    /// Two steps share a name.
    #[error("step {0} is defined twice")]
    DuplicateStep(StepId),
    /// An edge names a step the graph does not have.
    #[error("an edge names step {0}, which the graph does not have")]
    EdgeToNowhere(StepId),
    /// Forward edges lead from a step back to itself: a cycle with no bound. A way back must be a
    /// back edge, which has one.
    #[error(
        "the forward edges make a cycle through step {0}; a way back must be a bounded back edge"
    )]
    UnboundedCycle(StepId),
    /// A step has two forward edges. The runner takes one step at a time.
    #[error("step {0} has more than one forward edge; the runner takes one step at a time")]
    Branching(StepId),
    /// No path of forward edges from the first step reaches this one.
    #[error("step {0} cannot be reached from the first step")]
    Unreachable(StepId),
    /// A back edge to a later step.
    #[error(
        "the back edge of step {0} goes forward; a back edge goes to an earlier step or itself"
    )]
    BackEdgeGoesForward(StepId),
    /// A step has two back edges.
    #[error("step {0} has more than one back edge")]
    TwoBackEdges(StepId),
    /// A step is given untrusted input, private data and outside actions at once.
    #[error("step {0} has untrusted input, private data and outside actions at once (Rule of Two)")]
    RuleOfTwo(StepId),
}

/// Collects steps and edges, then checks them all in [`GraphBuilder::build`]. The first step added
/// is the graph's first; forward edges chain the steps in their execution order.
#[derive(Debug, Clone)]
#[must_use]
pub struct GraphBuilder {
    name: GraphName,
    steps: Vec<Step>,
    forward: Vec<(StepId, StepId)>,
    back: Vec<(StepId, StepId, LoopBound)>,
}

impl GraphBuilder {
    /// Starts a graph.
    pub const fn new(name: GraphName) -> Self {
        Self {
            name,
            steps: Vec::new(),
            forward: Vec::new(),
            back: Vec::new(),
        }
    }

    /// Adds a step. The first one added is where runs start.
    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    /// Adds a forward edge: after `from` passes, the run moves to `to`.
    pub fn edge(mut self, from: StepId, to: StepId) -> Self {
        self.forward.push((from, to));
        self
    }

    /// Adds a back edge from `from` to the earlier step `to`, taken at most `bound` times in a run.
    pub fn back_edge(mut self, from: StepId, to: StepId, bound: LoopBound) -> Self {
        self.back.push((from, to, bound));
        self
    }

    /// Checks everything and returns the graph, its steps in execution order.
    ///
    /// # Errors
    ///
    /// Returns the first [`GraphError`] found.
    pub fn build(self) -> Result<Graph, GraphError> {
        let Self {
            name,
            mut steps,
            forward,
            back,
        } = self;
        if steps.is_empty() {
            return Err(GraphError::Empty);
        }
        let mut known = BTreeSet::new();
        for step in &steps {
            if !known.insert(step.id.clone()) {
                return Err(GraphError::DuplicateStep(step.id.clone()));
            }
            if step.access.breaks_rule_of_two() {
                return Err(GraphError::RuleOfTwo(step.id.clone()));
            }
        }
        for (from, to) in forward
            .iter()
            .map(|(a, b)| (a, b))
            .chain(back.iter().map(|(a, b, _)| (a, b)))
        {
            for end in [from, to] {
                if !known.contains(end) {
                    return Err(GraphError::EdgeToNowhere(end.clone()));
                }
            }
        }
        for (from, to, bound) in back {
            let step = steps
                .iter_mut()
                .find(|step| step.id == from)
                .ok_or_else(|| GraphError::EdgeToNowhere(from.clone()))?;
            if step.back_edge.is_some() {
                return Err(GraphError::TwoBackEdges(from));
            }
            step.back_edge = Some(BackEdge { target: to, bound });
        }
        let order = execution_order(&steps, &forward)?;
        let mut by_id: BTreeMap<StepId, Step> = steps
            .into_iter()
            .map(|step| (step.id.clone(), step))
            .collect();
        let ordered: Vec<Step> = order.iter().filter_map(|id| by_id.remove(id)).collect();
        for (index, step) in ordered.iter().enumerate() {
            if let Some(edge) = &step.back_edge {
                let target = ordered.iter().position(|other| other.id == edge.target);
                if target.is_none_or(|target| target > index) {
                    return Err(GraphError::BackEdgeGoesForward(step.id.clone()));
                }
            }
        }
        Ok(Graph {
            name,
            steps: ordered,
        })
    }
}

/// The steps in the order the forward edges chain them from the first step: refuses branching,
/// cycles and steps the chain does not reach.
fn execution_order(
    steps: &[Step],
    forward: &[(StepId, StepId)],
) -> Result<Vec<StepId>, GraphError> {
    let mut next: BTreeMap<&StepId, &StepId> = BTreeMap::new();
    for (from, to) in forward {
        if next.insert(from, to).is_some() {
            return Err(GraphError::Branching(from.clone()));
        }
    }
    // Each step has at most one successor, so a walk of more than `steps.len()` hops from any step
    // has repeated a step: it is on a cycle or leads into one.
    for step in steps {
        let mut at = &step.id;
        for _ in 0..steps.len() {
            match next.get(at) {
                Some(&to) => at = to,
                None => break,
            }
            if *at == step.id {
                return Err(GraphError::UnboundedCycle(step.id.clone()));
            }
        }
    }
    let mut order = Vec::with_capacity(steps.len());
    let mut at = steps.first().map(|step| &step.id);
    while let Some(id) = at {
        order.push(id.clone());
        at = next.get(id).copied();
    }
    if let Some(lost) = steps.iter().find(|step| !order.contains(&step.id)) {
        return Err(GraphError::Unreachable(lost.id.clone()));
    }
    Ok(order)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::graph::{Access, BounceBound, Checker, EdgeKind, GateName, StepKind, Text};
    use proptest::prelude::*;

    pub(crate) fn id(name: &str) -> StepId {
        StepId::parse(name).unwrap()
    }

    pub(crate) fn step(name: &str) -> Step {
        Step {
            id: id(name),
            kind: StepKind::Agent {
                prompt: Text::parse("do the work").unwrap(),
            },
            checker: Checker::Gate {
                check: GateName::parse("tests").unwrap(),
            },
            max_bounces: BounceBound::new(2).unwrap(),
            access: Access::default(),
            back_edge: None,
        }
    }

    pub(crate) fn builder() -> GraphBuilder {
        GraphBuilder::new(GraphName::parse("review").unwrap())
    }

    /// `a -> b -> c`, built the way the tests extend.
    pub(crate) fn chain() -> GraphBuilder {
        builder()
            .step(step("a"))
            .step(step("b"))
            .step(step("c"))
            .edge(id("a"), id("b"))
            .edge(id("b"), id("c"))
    }

    fn bound(count: u32) -> LoopBound {
        LoopBound::new(count).unwrap()
    }

    #[test]
    fn a_chain_with_a_bounded_back_edge_builds() {
        let graph = chain()
            .back_edge(id("c"), id("a"), bound(2))
            .build()
            .unwrap();
        let names: Vec<&str> = graph.steps().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"], "execution order");
        let edges: Vec<(&str, &str, EdgeKind)> = graph
            .edges()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.kind))
            .collect();
        assert_eq!(
            edges,
            [
                ("a", "b", EdgeKind::Forward),
                ("b", "c", EdgeKind::Forward),
                ("c", "a", EdgeKind::Back(bound(2))),
            ],
            "forward edges, then back edges"
        );
    }

    #[test]
    fn steps_added_out_of_order_run_in_edge_order() {
        let graph = builder()
            .step(step("a"))
            .step(step("c"))
            .step(step("b"))
            .edge(id("b"), id("c"))
            .edge(id("a"), id("b"))
            .build()
            .unwrap();
        let names: Vec<&str> = graph.steps().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            names,
            ["a", "b", "c"],
            "the chain decides, not the insertion order"
        );
    }

    #[test]
    fn an_empty_graph_is_refused() {
        assert_eq!(builder().build(), Err(GraphError::Empty), "no steps");
    }

    #[test]
    fn a_step_defined_twice_is_refused() {
        assert_eq!(
            builder().step(step("a")).step(step("a")).build(),
            Err(GraphError::DuplicateStep(id("a"))),
            "duplicate"
        );
    }

    #[test]
    fn an_edge_to_nowhere_is_refused() {
        for graph in [
            builder().step(step("a")).edge(id("a"), id("z")),
            builder().step(step("a")).edge(id("z"), id("a")),
            builder()
                .step(step("a"))
                .back_edge(id("a"), id("z"), bound(1)),
            builder()
                .step(step("a"))
                .back_edge(id("z"), id("a"), bound(1)),
        ] {
            assert_eq!(
                graph.build(),
                Err(GraphError::EdgeToNowhere(id("z"))),
                "z is not a step"
            );
        }
    }

    #[test]
    fn an_unbounded_cycle_is_refused() {
        assert_eq!(
            chain().edge(id("c"), id("a")).build(),
            Err(GraphError::UnboundedCycle(id("a"))),
            "c leads back to a with no bound"
        );
        assert_eq!(
            builder().step(step("a")).edge(id("a"), id("a")).build(),
            Err(GraphError::UnboundedCycle(id("a"))),
            "a step leading to itself"
        );
        assert_eq!(
            chain().edge(id("c"), id("b")).build(),
            Err(GraphError::UnboundedCycle(id("b"))),
            "a cycle that does not include the first step"
        );
    }

    #[test]
    fn a_step_with_two_ways_forward_is_refused() {
        assert_eq!(
            chain().edge(id("a"), id("c")).build(),
            Err(GraphError::Branching(id("a"))),
            "branching"
        );
    }

    #[test]
    fn an_unreachable_step_is_refused() {
        assert_eq!(
            builder()
                .step(step("a"))
                .step(step("b"))
                .step(step("c"))
                .edge(id("a"), id("b"))
                .build(),
            Err(GraphError::Unreachable(id("c"))),
            "nothing leads to c"
        );
        assert_eq!(
            builder()
                .step(step("a"))
                .step(step("b"))
                .step(step("c"))
                .edge(id("b"), id("c"))
                .edge(id("a"), id("b"))
                .edge(id("c"), id("c"))
                .build(),
            Err(GraphError::UnboundedCycle(id("c"))),
            "a cycle is named before reachability"
        );
        assert_eq!(
            builder()
                .step(step("a"))
                .step(step("b"))
                .edge(id("b"), id("a"))
                .build(),
            Err(GraphError::Unreachable(id("b"))),
            "the first step added is where runs start"
        );
    }

    #[test]
    fn a_back_edge_must_go_back_once() {
        assert_eq!(
            chain().back_edge(id("a"), id("c"), bound(1)).build(),
            Err(GraphError::BackEdgeGoesForward(id("a"))),
            "forward"
        );
        assert_eq!(
            chain()
                .back_edge(id("c"), id("a"), bound(1))
                .back_edge(id("c"), id("b"), bound(1))
                .build(),
            Err(GraphError::TwoBackEdges(id("c"))),
            "two on one step"
        );
        let graph = chain()
            .back_edge(id("b"), id("b"), bound(3))
            .build()
            .unwrap();
        assert_eq!(
            graph
                .steps()
                .get(1)
                .and_then(|s| s.back_edge.as_ref())
                .map(|e| e.bound),
            Some(bound(3)),
            "a step may loop to itself"
        );
    }

    #[test]
    fn the_rule_of_two_is_enforced() {
        let mut risky = step("b");
        risky.access = Access {
            untrusted_input: true,
            private_data: true,
            outside_actions: true,
        };
        assert_eq!(
            builder()
                .step(step("a"))
                .step(risky.clone())
                .edge(id("a"), id("b"))
                .build(),
            Err(GraphError::RuleOfTwo(id("b"))),
            "all three"
        );
        risky.access.outside_actions = false;
        assert!(
            builder()
                .step(step("a"))
                .step(risky)
                .edge(id("a"), id("b"))
                .build()
                .is_ok(),
            "two are allowed"
        );
    }

    #[test]
    fn a_graph_read_from_json_is_checked_like_a_built_one() {
        let graph = chain()
            .back_edge(id("c"), id("a"), bound(2))
            .build()
            .unwrap();
        let json = serde_json::to_value(&graph).unwrap();
        assert_eq!(
            serde_json::from_value::<Graph>(json.clone()).unwrap(),
            graph,
            "round trip"
        );
        let mut risky = json.clone();
        risky["steps"][0]["access"]["untrusted_input"] = true.into();
        risky["steps"][0]["access"]["private_data"] = true.into();
        risky["steps"][0]["access"]["outside_actions"] = true.into();
        assert!(
            serde_json::from_value::<Graph>(risky)
                .unwrap_err()
                .to_string()
                .contains("Rule of Two"),
            "the rule applies to what is read"
        );
        let mut nowhere = json.clone();
        nowhere["steps"][2]["back_edge"]["target"] = "zzz".into();
        assert!(
            serde_json::from_value::<Graph>(nowhere).is_err(),
            "an edge to nowhere"
        );
        let mut empty = json;
        empty["steps"] = serde_json::json!([]);
        assert!(serde_json::from_value::<Graph>(empty).is_err(), "no steps");
    }

    fn arbitrary_edges()
    -> impl Strategy<Value = (usize, Vec<(usize, usize)>, Vec<(usize, usize, u32)>)> {
        (1_usize..6).prop_flat_map(|n| {
            (
                Just(n),
                proptest::collection::vec((0..n, 0..n), 0..8),
                proptest::collection::vec((0..n, 0..n, 1_u32..5), 0..4),
            )
        })
    }

    proptest! {
        /// Whatever edges are thrown at the builder, a graph that comes out is a chain of every
        /// step with bounded, backward loops only: the rules hold for every built graph.
        #[test]
        fn every_graph_that_builds_is_a_bounded_chain(
            (n, forward, back) in arbitrary_edges(),
        ) {
            let names: Vec<StepId> = (0..n).map(|i| id(&format!("s{i}"))).collect();
            let mut b = builder();
            for name in &names {
                b = b.step(step(name.as_str()));
            }
            for (from, to) in forward {
                b = b.edge(names[from].clone(), names[to].clone());
            }
            for (from, to, count) in back {
                b = b.back_edge(names[from].clone(), names[to].clone(), bound(count));
            }
            if let Ok(graph) = b.build() {
                prop_assert_eq!(graph.steps().len(), n, "every step is reachable");
                prop_assert_eq!(&graph.steps()[0].id, &names[0], "runs start at the first step");
                let position = |id: &StepId| graph.steps().iter().position(|s| &s.id == id).unwrap();
                for (index, step) in graph.steps().iter().enumerate() {
                    if let Some(edge) = &step.back_edge {
                        prop_assert!(position(&edge.target) <= index, "back edges go back");
                    }
                }
                prop_assert_eq!(
                    serde_json::from_value::<Graph>(serde_json::to_value(&graph).unwrap()).unwrap(),
                    graph,
                    "and survive a round trip"
                );
            }
        }
    }
}
