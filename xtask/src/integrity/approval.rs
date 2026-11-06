//! Whether a person accepted a pull request's integrity findings.
//!
//! An approver listed in `.github/integrity-approvers` at the merge base (so a pull request cannot
//! name its own approvers) accepts them by adding the label `integrity-approved` after the pull
//! request's latest push and latest description edit. The push time is when the head commit first
//! ran a pull request workflow; the label events come from the pull request's timeline. A label
//! that predates the push, was removed, or was added by anyone else does not count, and a run for
//! a description edit treats any label as stale. A separate CI job also removes the label on every
//! push and description edit.

use anyhow::{Context, ensure};
use serde::Deserialize;

/// The file listing who may accept integrity findings, relative to the repository root.
pub(super) const APPROVERS: &str = ".github/integrity-approvers";

/// The label that accepts them.
pub(super) const LABEL: &str = "integrity-approved";

/// The pull request the check runs for, with the token CI gives it.
#[derive(Debug)]
pub(crate) struct PullRequest {
    pub(crate) api: String,
    pub(crate) repository: String,
    pub(crate) number: u64,
    pub(crate) token: String,
}

impl PullRequest {
    /// The pull request `number` in the repository CI names in `GITHUB_REPOSITORY`, with
    /// `GITHUB_TOKEN`, GitHub Actions' variables read through `lookup`.
    pub(crate) fn from_lookup(
        number: u64,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Self> {
        let variable = |name: &str| {
            lookup(name).with_context(|| format!("{name} is not set; is this GitHub Actions?"))
        };
        Ok(Self {
            api: lookup("GITHUB_API_URL").unwrap_or_else(|| "https://api.github.com".to_owned()),
            repository: variable("GITHUB_REPOSITORY")?,
            number,
            token: variable("GITHUB_TOKEN")?,
        })
    }
}

/// The logins in the approvers file's `text`: one per line, `#` starting a comment.
pub(super) fn approvers(text: &str) -> anyhow::Result<Vec<String>> {
    let logins: Vec<String> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|login| !login.is_empty())
        .map(str::to_owned)
        .collect();
    ensure!(!logins.is_empty(), "{APPROVERS} lists no approver");
    Ok(logins)
}

/// The verdict and why.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Approval {
    pub(super) approved: bool,
    pub(super) reason: String,
}

#[derive(Deserialize)]
struct Pull {
    head: Head,
    labels: Vec<Named>,
}

#[derive(Deserialize)]
struct Head {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

/// One pull request timeline event; only label events matter here.
#[derive(Debug, Deserialize)]
struct TimelineItem {
    event: String,
    created_at: Option<String>,
    actor: Option<Login>,
    label: Option<Named>,
}

#[derive(Debug, Deserialize)]
struct Login {
    login: String,
}

/// Decides, from GitHub's API read through `fetch`, whether an approver accepted the findings.
pub(super) fn approved(
    pull_request: &PullRequest,
    approvers: &[String],
    body_edited: bool,
    fetch: &mut impl FnMut(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<Approval> {
    let base = format!("{}/repos/{}", pull_request.api, pull_request.repository);
    let pull: Pull =
        serde_json::from_slice(&fetch(&format!("{base}/pulls/{}", pull_request.number))?)
            .context("could not read the pull request")?;
    let push = crate::github::push_of(&pull.head.sha, &mut |path| {
        serde_json::from_slice(&fetch(&format!("{base}/{path}"))?)
            .with_context(|| format!("could not read {path}"))
    })?;
    let mut events = Vec::new();
    for page in 1_u32.. {
        let batch: Vec<TimelineItem> = serde_json::from_slice(&fetch(&format!(
            "{base}/issues/{}/timeline?per_page=100&page={page}",
            pull_request.number
        ))?)
        .context("could not read the pull request's timeline")?;
        if batch.is_empty() {
            break;
        }
        events.extend(batch);
    }
    let pushed = push.as_ref().map(|push| push.at.as_str());
    let labels: Vec<&str> = pull
        .labels
        .iter()
        .map(|label| label.name.as_str())
        .collect();
    Ok(decide(&labels, &events, pushed, approvers, body_edited))
}

/// The verdict from the pull request's labels, its timeline events, when its head was pushed,
/// and who may approve. With `body_edited`, this run is for a description edit, which an earlier
/// label never approved. Timestamps are GitHub's UTC ISO 8601, which sort as text.
fn decide(
    labels: &[&str],
    events: &[TimelineItem],
    pushed: Option<&str>,
    approvers: &[String],
    body_edited: bool,
) -> Approval {
    let refuse = |reason: String| Approval {
        approved: false,
        reason,
    };
    if !labels.contains(&LABEL) {
        return refuse(format!(
            "failed; an approver listed in {APPROVERS} can accept these findings by adding the \
             label {LABEL}"
        ));
    }
    if body_edited {
        return refuse(format!(
            "failed; the description changed after {LABEL} was added, so it is stale; an \
             approver must add it again"
        ));
    }
    let Some(pushed) = pushed else {
        return refuse("failed; cannot tell when the head commit was pushed".to_owned());
    };
    let latest = events
        .iter()
        .filter(|event| {
            matches!(event.event.as_str(), "labeled" | "unlabeled")
                && event
                    .label
                    .as_ref()
                    .is_some_and(|label| label.name == LABEL)
        })
        .max_by(|a, b| a.created_at.cmp(&b.created_at));
    match latest {
        Some(TimelineItem {
            event,
            created_at: Some(at),
            actor: Some(actor),
            ..
        }) if event == "labeled" => {
            if !approvers.contains(&actor.login) {
                refuse(format!(
                    "failed; {LABEL} was added by {}, who is not listed in {APPROVERS}",
                    actor.login
                ))
            } else if at.as_str() <= pushed {
                refuse(format!(
                    "failed; {LABEL} was added before the latest push; it must be added again"
                ))
            } else {
                Approval {
                    approved: true,
                    reason: format!("accepted by {} with {LABEL}", actor.login),
                }
            }
        }
        _ => refuse(format!(
            "failed; no current {LABEL} label event from an approver"
        )),
    }
}

/// GETs `url` from GitHub's REST API with `token`.
pub(super) fn github_get(url: &str, token: &str) -> anyhow::Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .with_context(|| format!("GitHub refused {url}"))?;
    response
        .body_mut()
        .read_to_vec()
        .with_context(|| format!("could not read {url}"))
}

#[cfg(test)]
mod tests {
    use super::{PullRequest, TimelineItem, approved, approvers, decide, github_get};
    use crate::testing::serve_once;

    fn events(json: &str) -> Vec<TimelineItem> {
        serde_json::from_str(json).unwrap()
    }

    const TIMELINE: &str = r#"[
        {"event": "committed"},
        {"event": "labeled", "created_at": "2026-11-01T10:00:00Z", "actor": {"login": "snehilvj"}, "label": {"name": "integrity-approved"}},
        {"event": "labeled", "created_at": "2026-11-01T10:05:00Z", "actor": {"login": "someone"}, "label": {"name": "bug"}}
    ]"#;

    fn logins() -> Vec<String> {
        vec!["snehilvj".to_owned()]
    }

    #[test]
    fn accepts_a_label_an_approver_added_after_the_push() {
        let verdict = decide(
            &["integrity-approved"],
            &events(TIMELINE),
            Some("2026-11-01T09:00:00Z"),
            &logins(),
            false,
        );
        assert!(verdict.approved, "{verdict:?}");
        assert_eq!(
            verdict.reason, "accepted by snehilvj with integrity-approved",
            "named"
        );
    }

    #[test]
    fn refuses_every_other_label_history() {
        let cases = [
            (
                vec![],
                TIMELINE,
                Some("2026-11-01T09:00:00Z"),
                "can accept these findings",
            ),
            (
                vec!["integrity-approved"],
                TIMELINE,
                Some("2026-11-01T11:00:00Z"),
                "before the latest push",
            ),
            (
                vec!["integrity-approved"],
                TIMELINE,
                None,
                "cannot tell when",
            ),
            (
                vec!["integrity-approved"],
                r#"[{"event": "labeled", "created_at": "2026-11-01T10:00:00Z", "actor": {"login": "agent"}, "label": {"name": "integrity-approved"}}]"#,
                Some("2026-11-01T09:00:00Z"),
                "added by agent, who is not listed",
            ),
            (
                vec!["integrity-approved"],
                r#"[{"event": "labeled", "created_at": "2026-11-01T10:00:00Z", "actor": {"login": "snehilvj"}, "label": {"name": "integrity-approved"}},
                    {"event": "unlabeled", "created_at": "2026-11-01T10:01:00Z", "actor": {"login": "snehilvj"}, "label": {"name": "integrity-approved"}}]"#,
                Some("2026-11-01T09:00:00Z"),
                "no current integrity-approved label event",
            ),
        ];
        for (labels, timeline, pushed, expected) in cases {
            let verdict = decide(&labels, &events(timeline), pushed, &logins(), false);
            assert!(!verdict.approved, "{expected}: {verdict:?}");
            assert!(verdict.reason.contains(expected), "{expected}: {verdict:?}");
        }
    }

    #[test]
    fn reads_the_verdict_from_github() {
        let pull_request = PullRequest {
            api: "https://api.example".to_owned(),
            repository: "o/r".to_owned(),
            number: 7,
            token: "t".to_owned(),
        };
        let mut asked = Vec::new();
        let mut fetch = |url: &str| -> anyhow::Result<Vec<u8>> {
            asked.push(url.to_owned());
            Ok(match url {
                "https://api.example/repos/o/r/pulls/7" => {
                    br#"{"head": {"sha": "abc"}, "labels": [{"name": "integrity-approved"}]}"#.to_vec()
                }
                "https://api.example/repos/o/r/actions/runs?event=pull_request&head_sha=abc&per_page=100&page=1" => {
                    br#"{"workflow_runs": [{"created_at": "2026-11-01T09:30:00Z", "actor": {"login": "snehilvj"}}, {"created_at": "2026-11-01T09:00:00Z", "actor": {"login": "getabc-dev-agent[bot]"}}]}"#.to_vec()
                }
                "https://api.example/repos/o/r/issues/7/timeline?per_page=100&page=1" => TIMELINE.as_bytes().to_vec(),
                _ => b"[]".to_vec(),
            })
        };
        let verdict = approved(&pull_request, &logins(), false, &mut fetch).unwrap();
        assert!(verdict.approved, "{verdict:?}");
        assert!(
            asked.last().is_some_and(|url| url.ends_with("page=2")),
            "the timeline is read until an empty page: {asked:?}"
        );
    }

    #[test]
    fn reads_github_actions_variables() {
        let lookup = |name: &str| match name {
            "GITHUB_REPOSITORY" => Some("o/r".to_owned()),
            "GITHUB_TOKEN" => Some("t".to_owned()),
            _ => None,
        };
        let pull_request = PullRequest::from_lookup(7, lookup).unwrap();
        assert_eq!(
            (
                pull_request.api.as_str(),
                pull_request.repository.as_str(),
                pull_request.number
            ),
            ("https://api.github.com", "o/r", 7),
            "the public API by default"
        );
        let error = PullRequest::from_lookup(7, |_| None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("GITHUB_REPOSITORY is not set"), "{error}");
    }

    #[test]
    fn calls_the_github_api_with_the_token() {
        let (url, server) = serve_once("200 OK", r#"{"ok": true}"#);
        assert_eq!(
            github_get(&format!("{url}/x"), "secret").unwrap(),
            br#"{"ok": true}"#,
            "body"
        );
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /x HTTP/1.1"), "{request}");
        assert!(
            request.contains("authorization: Bearer secret"),
            "{request}"
        );
        let (missing, refused) = serve_once("404 Not Found", "{}");
        assert!(
            github_get(&format!("{missing}/y"), "secret").is_err(),
            "a 404 is an error"
        );
        refused.join().unwrap();
    }

    #[test]
    fn a_description_edit_makes_the_label_stale() {
        let verdict = decide(
            &["integrity-approved"],
            &events(TIMELINE),
            Some("2026-11-01T09:00:00Z"),
            &logins(),
            true,
        );
        assert!(!verdict.approved, "{verdict:?}");
        assert!(
            verdict.reason.contains("the description changed"),
            "{verdict:?}"
        );
    }

    #[test]
    fn reads_the_approvers_file() {
        assert_eq!(
            approvers("# who\nsnehilvj # owner\n\n").unwrap(),
            ["snehilvj"],
            "comments dropped"
        );
        assert!(
            approvers("# nobody\n").is_err(),
            "an empty list is an error"
        );
    }
}
