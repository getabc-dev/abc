//! What a run's steps report and what a person answers: the vocabulary of `spec/runner/`.
//!
//! [`Outcome`] is the spec's `Outcome` and [`Answer`] its `Answer`. A checker's verdict carries the
//! confidence it had (decision 33), which the spec leaves out because it does not change what the
//! runner does.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::identity::InvalidValue;

/// Whose work a lease is for: first the maker, then a checker with fresh eyes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The step's work.
    Maker,
    /// The judgement of that work.
    Checker,
}

/// What a worker reports for a lease (spec `Outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The maker finished: the step moves to its checker.
    Made,
    /// The step passed.
    Pass,
    /// The step passed and asks to go back along its back edge, if its bound allows.
    Loop,
    /// The checker sends the work back to the maker, if the step's bounce bound allows.
    Bounce,
    /// The checker asks a person.
    Escalate,
}

impl Outcome {
    /// The phase whose lease may report this outcome (spec `outcomes`).
    #[must_use]
    pub const fn phase(self) -> Phase {
        match self {
            Self::Made => Phase::Maker,
            Self::Pass | Self::Loop | Self::Bounce | Self::Escalate => Phase::Checker,
        }
    }
}

/// What a checker decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The work is good.
    Pass,
    /// The work is good, and the run should go back along the step's back edge.
    Loop,
    /// The work needs another go.
    Bounce,
    /// A person must decide.
    Escalate,
}

/// How sure a checker is, as a percentage from 0 to 100.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(try_from = "u8", into = "u8")]
#[ts(type = "number")]
#[schemars(with = "u8", extend("minimum" = 0, "maximum" = 100))]
pub struct Confidence(u8);

impl Confidence {
    /// Checks the range.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] above 100.
    pub const fn new(percent: u8) -> Result<Self, InvalidValue> {
        if percent <= 100 {
            Ok(Self(percent))
        } else {
            Err(InvalidValue {
                field: "confidence",
                rule: "0 to 100",
            })
        }
    }

    /// The percentage.
    #[must_use]
    pub const fn percent(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Confidence {
    type Error = InvalidValue;

    fn try_from(percent: u8) -> Result<Self, InvalidValue> {
        Self::new(percent)
    }
}

impl From<Confidence> for u8 {
    fn from(confidence: Confidence) -> Self {
        confidence.0
    }
}

/// A checker's decision: its verdict and how sure it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct Decision {
    /// What it decided.
    pub verdict: Verdict,
    /// How sure it was.
    pub confidence: Confidence,
}

/// A step's result as a lease reports it: the maker's, or a checker's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepResult {
    /// The maker finished.
    Made,
    /// A checker decided.
    Judged(Decision),
}

impl StepResult {
    /// The spec's outcome for this result.
    #[must_use]
    pub const fn outcome(self) -> Outcome {
        match self {
            Self::Made => Outcome::Made,
            Self::Judged(Decision { verdict, .. }) => match verdict {
                Verdict::Pass => Outcome::Pass,
                Verdict::Loop => Outcome::Loop,
                Verdict::Bounce => Outcome::Bounce,
                Verdict::Escalate => Outcome::Escalate,
            },
        }
    }
}

/// A person's answer to an escalation (spec `Answer`): the run resumes at the step it parked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// The step's maker works again.
    Retry,
    /// The step's work counts as passed.
    Accept,
    /// The run ends.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_outcome_belongs_to_one_phase() {
        assert_eq!(
            Outcome::Made.phase(),
            Phase::Maker,
            "the maker reports Made"
        );
        for outcome in [
            Outcome::Pass,
            Outcome::Loop,
            Outcome::Bounce,
            Outcome::Escalate,
        ] {
            assert_eq!(
                outcome.phase(),
                Phase::Checker,
                "{outcome:?} is a checker's"
            );
        }
    }

    #[test]
    fn a_result_names_its_spec_outcome() {
        assert_eq!(StepResult::Made.outcome(), Outcome::Made, "made");
        let judged = |verdict| {
            StepResult::Judged(Decision {
                verdict,
                confidence: Confidence::new(80).unwrap(),
            })
            .outcome()
        };
        assert_eq!(judged(Verdict::Pass), Outcome::Pass, "pass");
        assert_eq!(judged(Verdict::Loop), Outcome::Loop, "loop");
        assert_eq!(judged(Verdict::Bounce), Outcome::Bounce, "bounce");
        assert_eq!(judged(Verdict::Escalate), Outcome::Escalate, "escalate");
    }

    #[test]
    fn confidence_is_a_percentage() {
        assert_eq!(Confidence::new(100).unwrap().percent(), 100, "the top");
        assert!(Confidence::new(101).is_err(), "over");
        assert!(
            serde_json::from_str::<Confidence>("101").is_err(),
            "checked from JSON"
        );
        assert_eq!(u8::from(Confidence::try_from(5).unwrap()), 5, "round trip");
    }

    #[test]
    fn results_have_a_stable_wire_form() {
        let result = StepResult::Judged(Decision {
            verdict: Verdict::Bounce,
            confidence: Confidence::new(70).unwrap(),
        });
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({"type": "judged", "verdict": "bounce", "confidence": 70}),
            "judged"
        );
        assert_eq!(
            serde_json::to_value(StepResult::Made).unwrap(),
            serde_json::json!({"type": "made"}),
            "made"
        );
    }
}
