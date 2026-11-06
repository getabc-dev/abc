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

/// Runs a page of the runs API holds.
const RUNS_PER_PAGE: usize = 100;

/// The push of `head`, read from its earliest `pull_request` workflow run, whose actor is who
/// pushed it. Later runs for the same head come from labels, description edits and re-runs, and
/// name whoever made them: #32's `integrity-approved` re-run was the newest run, so reading the
/// newest named the maintainer as the pusher. `page` answers a path under the repository with
/// its JSON; every page is read. Nothing when no run has started yet.
pub(crate) fn push_of(
    head: &str,
    page: &mut impl FnMut(&str) -> anyhow::Result<Value>,
) -> anyhow::Result<Option<Push>> {
    let mut earliest: Option<Push> = None;
    for number in 1_u32.. {
        let path = format!(
            "actions/runs?event=pull_request&head_sha={head}&per_page={RUNS_PER_PAGE}&page={number}"
        );
        let answer = page(&path)?;
        let runs = answer
            .get("workflow_runs")
            .and_then(Value::as_array)
            .with_context(|| format!("GitHub answered {path} without workflow_runs"))?;
        for run in runs {
            let field = |pointer: &str| {
                run.pointer(pointer)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .with_context(|| format!("a workflow run for {head} has no {pointer}"))
            };
            let push = Push {
                pusher: field("/actor/login")?,
                at: field("/created_at")?,
            };
            // GitHub lists runs newest first, so of two in the same second the later listed is
            // the older: a push and a label's run can share one.
            if earliest.as_ref().is_none_or(|first| push.at <= first.at) {
                earliest = Some(push);
            }
        }
        if runs.len() < RUNS_PER_PAGE {
            break;
        }
    }
    Ok(earliest)
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
