//! GitHub's API as the agent App, for `cargo xtask pr`.
//!
//! A command takes one App token from the broker, makes every call it needs with it, and revokes
//! it when done ([`with_token`]): the broker gives a session at most 120 tokens an hour, and a
//! command that ran gh once per call spent one per call. Calls go through a [`Send`] function, so
//! tests answer them with recorded replies.

use std::path::Path;

use anyhow::{Context as _, bail};
use serde_json::Value;

/// One call to GitHub's API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) method: &'static str,
    /// The path under the API's root, with its query string.
    pub(crate) path: String,
    pub(crate) body: Option<Value>,
}

/// What makes the calls: answers a request with the JSON GitHub sent back, failing on any status
/// that is not a success.
pub(crate) type Send<'a> = dyn FnMut(&Request) -> anyhow::Result<Value> + 'a;

/// The repository a checkout's `origin` names on github.com.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repo {
    pub(crate) owner: String,
    pub(crate) name: String,
}

impl Repo {
    /// The repository in `url`, an HTTPS or SSH URL of github.com.
    pub(crate) fn parse(url: &str) -> anyhow::Result<Self> {
        let path = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("git@github.com:"))
            .or_else(|| url.strip_prefix("ssh://git@github.com/"))
            .with_context(|| format!("origin is not a github.com repository: {url}"))?;
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        match path.split('/').collect::<Vec<_>>().as_slice() {
            [owner, name] if !owner.is_empty() && !name.is_empty() => Ok(Self {
                owner: (*owner).to_owned(),
                name: (*name).to_owned(),
            }),
            _ => bail!("origin is not a github.com repository: {url}"),
        }
    }

    /// The repository `origin` names in the checkout at `root`.
    pub(crate) fn of(root: &Path) -> anyhow::Result<Self> {
        Self::parse(crate::git::git(root, &["remote", "get-url", "origin"])?.trim())
    }

    /// A REST path under this repository, such as `pulls/7`.
    pub(crate) fn path(&self, rest: &str) -> String {
        format!("/repos/{}/{}/{rest}", self.owner, self.name)
    }
}

/// A REST call without a body.
pub(crate) fn get(send: &mut Send<'_>, path: String) -> anyhow::Result<Value> {
    send(&Request {
        method: "GET",
        path,
        body: None,
    })
}

/// A GraphQL query with its variables; its `data`, or GitHub's errors.
pub(crate) fn graphql(
    send: &mut Send<'_>,
    query: &str,
    variables: &Value,
) -> anyhow::Result<Value> {
    let mut answer = send(&Request {
        method: "POST",
        path: "/graphql".to_owned(),
        body: Some(serde_json::json!({"query": query, "variables": variables})),
    })?;
    if let Some(errors) = answer.get("errors").and_then(Value::as_array) {
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .collect();
        bail!(
            "GitHub's GraphQL API refused the query: {}",
            messages.join("; ")
        );
    }
    answer
        .get_mut("data")
        .map(Value::take)
        .context("GitHub's GraphQL API answered without data")
}

/// The push that made a commit a pull request's head: who pushed it and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Push {
    pub(crate) pusher: String,
    /// GitHub's UTC ISO 8601 time, which sorts as text.
    pub(crate) at: String,
}

/// A pull request's head, whose push [`push_of`] reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Head<'a> {
    pub(crate) sha: &'a str,
    /// The repository the head branch is in, `owner/name`: a fork's for a fork's pull request.
    pub(crate) repository: &'a str,
    pub(crate) branch: &'a str,
}

/// How many items GitHub lists per page at most, as these reads ask for.
const PER_PAGE: usize = 100;

/// The workflow whose runs are the pull request's pushes: it runs on `pull_request` with
/// GitHub's default activity types alone (opened, synchronize, reopened), so a label, a
/// description edit or a review never starts one. A test keeps `.github/workflows/ci.yml` so.
pub(crate) const PUSH_WORKFLOW: &str = "ci.yml";

/// Every item of the pull request `number`'s timeline, oldest first, read through `page` (a path
/// under the repository answered with its JSON).
pub(crate) fn timeline(
    number: u64,
    page: &mut impl FnMut(&str) -> anyhow::Result<Value>,
) -> anyhow::Result<Vec<Value>> {
    let mut items = Vec::new();
    for index in 1_u32.. {
        let path = format!("issues/{number}/timeline?per_page={PER_PAGE}&page={index}");
        let answer = page(&path)?;
        let batch = answer
            .as_array()
            .with_context(|| format!("GitHub answered {path} with no list"))?;
        items.extend(batch.iter().cloned());
        if batch.len() < PER_PAGE {
            break;
        }
    }
    Ok(items)
}

/// The latest push that made `head` the pull request's head, from the pull request's own
/// records: the [`PUSH_WORKFLOW`] runs for `head` on its head branch, its `head_ref_force_pushed`
/// events and its `reopened` events in `timeline`, whichever came last; `page` answers a path
/// under the repository with its JSON, and every page is read. Nothing when none of them records
/// `head` itself, as when no run has started yet: the push is then unknown.
///
/// The timeline cannot give it alone: its `committed` items carry only the commit's own dates,
/// which its author sets, and no pusher. Any run cannot either: #32's `integrity-approved` label
/// started runs that named the maintainer, and the earliest run for a head can belong to another
/// pull request with the same head, or be older than a force push that brought the head back
/// after an approval.
pub(crate) fn push_of(
    head: Head<'_>,
    timeline: &[Value],
    page: &mut impl FnMut(&str) -> anyhow::Result<Value>,
) -> anyhow::Result<Option<Push>> {
    let mut latest: Option<Push> = None;
    let mut keep = |push: Push| {
        // Of two in the same second, the first read stays: GitHub lists runs newest first.
        if latest.as_ref().is_none_or(|last| push.at > last.at) {
            latest = Some(push);
        }
    };
    let mut recorded = false;
    for number in 1_u32.. {
        let path = format!(
            "actions/workflows/{PUSH_WORKFLOW}/runs?event=pull_request&head_sha={}&per_page={PER_PAGE}&page={number}",
            head.sha
        );
        let answer = page(&path)?;
        let runs = answer
            .get("workflow_runs")
            .and_then(Value::as_array)
            .with_context(|| format!("GitHub answered {path} without workflow_runs"))?;
        for run in runs {
            let text = |pointer: &str| run.pointer(pointer).and_then(Value::as_str);
            // The same commit can head another pull request, from a fork or another branch.
            if text("/head_repository/full_name") != Some(head.repository)
                || text("/head_branch") != Some(head.branch)
                || text("/head_sha") != Some(head.sha)
            {
                continue;
            }
            recorded = true;
            keep(push(run, "a workflow run")?);
        }
        if runs.len() < PER_PAGE {
            break;
        }
    }
    for item in timeline {
        match item.get("event").and_then(Value::as_str) {
            Some("head_ref_force_pushed") => {
                // A force push of another commit made another head, not this one.
                if item.get("commit_id").and_then(Value::as_str) == Some(head.sha) {
                    recorded = true;
                    keep(push(item, "a head_ref_force_pushed event")?);
                }
            }
            Some("reopened") => keep(push(item, "a reopened event")?),
            _ => {}
        }
    }
    Ok(latest.filter(|_| recorded))
}

/// The push `item` (a run or a timeline event, `what`) records: its actor and creation time.
fn push(item: &Value, what: &str) -> anyhow::Result<Push> {
    let field = |pointer: &str| {
        item.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("{what} has no {pointer}"))
    };
    Ok(Push {
        pusher: field("/actor/login")?,
        at: field("/created_at")?,
    })
}

/// Runs `work` with a token from `mint`, then revokes the token with `revoke`, whatever `work`
/// did. A revocation that fails is reported, not fatal: the token expires within the hour.
pub(crate) fn with_token<T>(
    mint: impl FnOnce() -> anyhow::Result<String>,
    revoke: impl FnOnce(&str) -> anyhow::Result<()>,
    report: &mut impl std::io::Write,
    work: impl FnOnce(&str) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let token = mint()?;
    let result = work(&token);
    if let Err(error) = revoke(&token) {
        writeln!(
            report,
            "xtask pr: could not revoke the token; it expires within the hour: {error:#}"
        )?;
    }
    result
}

/// `builder` with `token` and GitHub's API headers, reading every status as an answer.
fn authorized<B>(builder: ureq::RequestBuilder<B>, token: &str) -> ureq::RequestBuilder<B> {
    builder
        .config()
        .http_status_as_error(false)
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// Sends requests to the API at `api` with `token`.
pub(crate) fn http<'a>(
    api: &'a str,
    token: &'a str,
) -> impl FnMut(&Request) -> anyhow::Result<Value> + 'a {
    move |request: &Request| {
        let url = format!("{api}{}", request.path);
        let sent = match (request.method, &request.body) {
            ("GET", None) => authorized(ureq::get(&url), token).call(),
            ("POST", Some(body)) => authorized(ureq::post(&url), token)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            ("PATCH", Some(body)) => authorized(ureq::patch(&url), token)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            (method, _) => bail!("xtask pr makes no {method} call with that body"),
        };
        let mut response =
            sent.with_context(|| format!("could not reach GitHub for {} {url}", request.method))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("could not read GitHub's answer to {url}"))?;
        if !(200..300).contains(&status) {
            bail!(
                "GitHub answered {} {} with HTTP {status}: {}",
                request.method,
                request.path,
                text.trim()
            );
        }
        serde_json::from_str(&text).with_context(|| format!("GitHub's answer to {url} is not JSON"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// A full page of the timeline is followed by the next; a shorter one is the last.
    #[test]
    fn reads_the_timeline_page_by_page() {
        let mut asked = Vec::new();
        let items = super::timeline(7, &mut |path: &str| {
            asked.push(path.to_owned());
            let count = if path.ends_with("&page=1") { 100 } else { 1 };
            Ok(serde_json::Value::Array(vec![
                json!({"event": "labeled"});
                count
            ]))
        })
        .unwrap();
        assert_eq!(items.len(), 101);
        assert_eq!(
            asked,
            [
                "issues/7/timeline?per_page=100&page=1",
                "issues/7/timeline?per_page=100&page=2"
            ]
        );
    }

    /// A force push names the head it made: one to this head records its push though no run
    /// started; one to another commit does not.
    #[test]
    fn a_force_push_records_only_the_head_it_made() {
        let forced = |commit: &str| {
            [json!({
                "event": "head_ref_force_pushed",
                "commit_id": commit,
                "actor": {"login": "snehilvj"},
                "created_at": "2026-11-05T10:00:00Z",
            })]
        };
        let head = super::Head {
            sha: "abc",
            repository: "o/r",
            branch: "x",
        };
        let mut no_runs = |_: &str| Ok(json!({"workflow_runs": []}));
        assert_eq!(
            super::push_of(head, &forced("abc"), &mut no_runs).unwrap(),
            Some(super::Push {
                pusher: "snehilvj".to_owned(),
                at: "2026-11-05T10:00:00Z".to_owned(),
            })
        );
        assert_eq!(
            super::push_of(head, &forced("def"), &mut no_runs).unwrap(),
            None,
            "a force push to another commit is no push of this head"
        );
    }

    /// A newer force push of another commit is not the head's push: the head's own, earlier one
    /// stays the latest.
    #[test]
    fn a_newer_force_push_of_another_commit_is_not_the_latest_push() {
        let timeline = [
            json!({
                "event": "head_ref_force_pushed",
                "commit_id": "abc",
                "actor": {"login": "alice"},
                "created_at": "2026-11-05T09:00:00Z",
            }),
            json!({
                "event": "head_ref_force_pushed",
                "commit_id": "def",
                "actor": {"login": "bob"},
                "created_at": "2026-11-05T10:00:00Z",
            }),
        ];
        let push = super::push_of(
            super::Head {
                sha: "abc",
                repository: "o/r",
                branch: "x",
            },
            &timeline,
            &mut |_: &str| Ok(json!({"workflow_runs": []})),
        )
        .unwrap();
        assert_eq!(
            push,
            Some(super::Push {
                pusher: "alice".to_owned(),
                at: "2026-11-05T09:00:00Z".to_owned(),
            })
        );
    }

    /// A pull request reopened after its head's run, with a branch pushed while it was closed,
    /// counts the reopening as its latest push.
    #[test]
    fn a_reopening_after_the_run_is_the_latest_push() {
        let run = json!({
            "head_sha": "abc",
            "head_branch": "x",
            "head_repository": {"full_name": "o/r"},
            "actor": {"login": "getabc-dev-agent[bot]"},
            "created_at": "2026-11-05T09:00:00Z",
        });
        let timeline = [json!({
            "event": "reopened",
            "actor": {"login": "snehilvj"},
            "created_at": "2026-11-05T10:00:00Z",
        })];
        let push = super::push_of(
            super::Head {
                sha: "abc",
                repository: "o/r",
                branch: "x",
            },
            &timeline,
            &mut |_: &str| Ok(json!({"workflow_runs": [run.clone()]})),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            push,
            super::Push {
                pusher: "snehilvj".to_owned(),
                at: "2026-11-05T10:00:00Z".to_owned(),
            }
        );
    }

    /// [`push_of`] reads pushes from [`PUSH_WORKFLOW`]'s runs, so nothing but a push or a
    /// reopen may start one: GitHub's default `pull_request` activity types.
    #[test]
    fn the_push_workflow_runs_on_pushes_alone() {
        let text = crate::testing::read_in_repository(&format!(
            ".github/workflows/{}",
            super::PUSH_WORKFLOW
        ));
        let workflow = &yaml_rust2::YamlLoader::load_from_str(&text).unwrap()[0];
        let trigger = &workflow["on"]["pull_request"];
        assert!(
            !trigger.is_badvalue(),
            "{} runs on pull_request",
            super::PUSH_WORKFLOW
        );
        assert!(
            trigger["types"].is_badvalue(),
            "{} keeps the default types (opened, synchronize, reopened); a label or edit that \
             started it would read as a push: {trigger:?}",
            super::PUSH_WORKFLOW
        );
    }

    use super::{Repo, Request, get, graphql, http, with_token};
    use crate::testing::serve;

    #[test]
    fn reads_the_repository_from_origin() {
        let repo = Repo {
            owner: "getabc-dev".to_owned(),
            name: "abc".to_owned(),
        };
        for url in [
            "https://github.com/getabc-dev/abc.git",
            "https://github.com/getabc-dev/abc",
            "git@github.com:getabc-dev/abc.git",
            "ssh://git@github.com/getabc-dev/abc/",
        ] {
            assert_eq!(Repo::parse(url).unwrap(), repo, "{url}");
        }
        for url in [
            "https://gitlab.com/o/r.git",
            "https://github.com/o",
            "https://github.com/o/r/x",
            "https://github.com//r.git",
            "https://github.com/o/.git",
        ] {
            assert!(Repo::parse(url).is_err(), "{url}");
        }
        assert_eq!(repo.path("pulls/7"), "/repos/getabc-dev/abc/pulls/7");
        let temp = tempfile::tempdir().unwrap();
        let checkout = crate::testing::repository(&temp.path().join("r"));
        crate::testing::git(
            &checkout,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        assert_eq!(Repo::of(&checkout).unwrap().path("x"), "/repos/o/r/x");
    }

    #[test]
    fn a_graphql_answer_is_its_data_or_its_errors() {
        let mut requests = Vec::new();
        let mut answers = vec![
            json!({"errors": [{"message": "no such field"}, {"message": "and another"}]}),
            json!({"data": {"viewer": {"login": "a"}}}),
            json!({}),
        ]
        .into_iter();
        let mut send = |request: &Request| {
            requests.push(request.clone());
            Ok(answers.next().unwrap())
        };
        let refused = format!(
            "{:#}",
            graphql(&mut send, "query { x }", &json!({})).unwrap_err()
        );
        assert_eq!(
            refused,
            "GitHub's GraphQL API refused the query: no such field; and another"
        );
        assert_eq!(
            graphql(&mut send, "query { viewer { login } }", &json!({"a": 1})).unwrap(),
            json!({"viewer": {"login": "a"}})
        );
        assert!(graphql(&mut send, "q", &json!({})).is_err(), "no data");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/graphql");
        assert_eq!(
            requests[1].body,
            Some(json!({"query": "query { viewer { login } }", "variables": {"a": 1}}))
        );
    }

    #[test]
    fn one_token_serves_every_call_and_is_revoked_after() {
        let mut revoked = Vec::new();
        let mut minted = 0;
        let mut report = Vec::new();
        let result = with_token(
            || {
                minted += 1;
                Ok("ghs_t".to_owned())
            },
            |token| {
                revoked.push(token.to_owned());
                anyhow::bail!("offline")
            },
            &mut report,
            |token| -> anyhow::Result<String> { anyhow::bail!("failed with {token}") },
        );
        assert_eq!(format!("{:#}", result.unwrap_err()), "failed with ghs_t");
        assert_eq!((minted, revoked), (1, vec!["ghs_t".to_owned()]));
        assert!(
            String::from_utf8(report)
                .unwrap()
                .contains("could not revoke the token; it expires within the hour: offline"),
            "revoked even when the work failed"
        );
        let refused = with_token(
            || anyhow::bail!("broker down"),
            |_| panic!("nothing to revoke"),
            &mut Vec::new(),
            |_| -> anyhow::Result<()> { panic!("no work without a token") },
        );
        assert!(refused.is_err());
    }

    #[test]
    fn calls_the_api_with_the_token_and_reads_its_answers() {
        let (url, server) = serve(vec![
            ("200 OK", r#"{"number": 7}"#),
            ("201 Created", r#"{"number": 8}"#),
            (
                "422 Unprocessable Entity",
                r#"{"message": "Validation Failed"}"#,
            ),
            ("200 OK", "not json"),
        ]);
        let mut send = http(&url, "ghs_t");
        assert_eq!(
            get(&mut send, "/repos/o/r/pulls/7".to_owned()).unwrap(),
            json!({"number": 7})
        );
        let post = Request {
            method: "POST",
            path: "/repos/o/r/pulls".to_owned(),
            body: Some(json!({"title": "t"})),
        };
        assert_eq!(send(&post).unwrap(), json!({"number": 8}));
        let refused = format!(
            "{:#}",
            send(&Request {
                method: "PATCH",
                ..post.clone()
            })
            .unwrap_err()
        );
        assert_eq!(
            refused,
            r#"GitHub answered PATCH /repos/o/r/pulls with HTTP 422: {"message": "Validation Failed"}"#
        );
        assert!(send(&post).is_err(), "an answer that is not JSON");
        let unsupported = Request {
            method: "DELETE",
            body: None,
            ..post
        };
        assert!(send(&unsupported).is_err(), "no call it does not make");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /repos/o/r/pulls/7 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer ghs_t"),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("POST /repos/o/r/pulls "),
            "{}",
            requests[1]
        );
        assert!(requests[1].ends_with(r#"{"title":"t"}"#), "{}", requests[1]);
        assert!(requests[2].starts_with("PATCH "), "{}", requests[2]);
    }
}
