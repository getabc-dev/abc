//! `cargo xtask pr checks <number>`: what is failing or still running on a pull request, with
//! the annotations the gates left ([`crate::annotation`]), read from GitHub's API, which agent VMs
//! reach, rather than from job logs, which they do not.
//!
//! Two calls: one REST call for the latest merge-queue run of the pull request, then one GraphQL
//! query for the check runs and annotations of the head and of that merge-queue commit.

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::{self, Repo, Send};

/// The check runs of one commit, with their annotations.
const COMMIT_FRAGMENT: &str = "fragment runs on Commit {
  oid
  checkSuites(first: 30) {
    nodes {
      workflowRun { workflow { name } }
      checkRuns(first: 50) {
        nodes {
          name status conclusion detailsUrl
          annotations(first: 30) {
            nodes { path title message annotationLevel location { start { line } end { line } } }
          }
        }
      }
    }
  }
}";

/// The GraphQL query for pull request `$number`, and for the merge-queue commit `$queue` when
/// `queue` is set.
fn query(queue: bool) -> String {
    let (variable, field) = if queue {
        (
            ", $queue: GitObjectID!",
            "\n    queue: object(oid: $queue) { ...runs }",
        )
    } else {
        ("", "")
    };
    format!(
        "{COMMIT_FRAGMENT}\nquery($owner: String!, $name: String!, $number: Int!{variable}) {{\n  \
         repository(owner: $owner, name: $name) {{\n    pullRequest(number: $number) {{ commits(last: \
         1) {{ nodes {{ commit {{ ...runs }} }} }} }}{field}\n  }}\n}}"
    )
}

#[derive(Debug, Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    oid: String,
    check_suites: Nodes<Suite>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Suite {
    workflow_run: Option<WorkflowRun>,
    check_runs: Nodes<CheckRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    workflow: Workflow,
}

#[derive(Debug, Deserialize)]
struct Workflow {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    annotations: Nodes<Note>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Note {
    path: String,
    title: Option<String>,
    message: String,
    annotation_level: Option<String>,
    location: Location,
}

#[derive(Debug, Deserialize)]
struct Location {
    start: Line,
    end: Line,
}

#[derive(Debug, Deserialize)]
struct Line {
    line: u64,
}

/// Where a check run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Failing,
    Cancelled,
    Running,
    Passed,
}

impl CheckRun {
    fn state(&self) -> State {
        if self.status != "COMPLETED" {
            return State::Running;
        }
        match self.conclusion.as_deref() {
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED" | "STALE") => State::Passed,
            Some("CANCELLED") => State::Cancelled,
            _ => State::Failing,
        }
    }
}

/// The latest merge-queue commit of pull request `number`, with its branch, from the merge-queue
/// runs GitHub lists newest first.
fn latest_queue_commit(runs: &Value, number: u64) -> Option<(String, String)> {
    let marker = format!("/pr-{number}-");
    runs.get("workflow_runs")?
        .as_array()?
        .iter()
        .find_map(|run| {
            let branch = run.get("head_branch")?.as_str()?;
            let sha = run.get("head_sha")?.as_str()?;
            (branch.starts_with("gh-readonly-queue/") && branch.contains(&marker))
                .then(|| (sha.to_owned(), branch.to_owned()))
        })
}

/// Reads the checks of pull request `number` through `send` and writes them to `out`. Returns
/// whether every check of the head, and of its latest merge-queue run, finished and passed.
pub(crate) fn run(
    repo: &Repo,
    number: u64,
    send: &mut Send<'_>,
    out: &mut String,
) -> anyhow::Result<bool> {
    let runs = github::get(
        send,
        repo.path("actions/runs?event=merge_group&per_page=100"),
    )?;
    let queue = latest_queue_commit(&runs, number);
    let mut variables = serde_json::Map::new();
    variables.insert("owner".to_owned(), json!(repo.owner));
    variables.insert("name".to_owned(), json!(repo.name));
    variables.insert("number".to_owned(), json!(number));
    if let Some((sha, _)) = &queue {
        variables.insert("queue".to_owned(), json!(sha));
    }
    let mut data = github::graphql(send, &query(queue.is_some()), &Value::Object(variables))?;
    let head: Commit = serde_json::from_value(
        data.pointer_mut("/repository/pullRequest/commits/nodes/0/commit")
            .map(Value::take)
            .with_context(|| format!("GitHub has no pull request #{number} with commits"))?,
    )
    .context("GitHub's check runs are not as expected")?;
    let mut clean = describe(&format!("#{number} head"), &head, out);
    match &queue {
        Some((_, branch)) => {
            let queued: Commit = serde_json::from_value(
                data.pointer_mut("/repository/queue")
                    .map(Value::take)
                    .context("GitHub has no merge-queue commit")?,
            )
            .context("GitHub's merge-queue check runs are not as expected")?;
            clean &= describe(&format!("merge queue {branch}"), &queued, out);
        }
        None => out.push_str("merge queue: no runs for this pull request\n"),
    }
    Ok(clean)
}

/// Adds `line` to `out`, ending it.
fn say(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// Writes the failing, cancelled and running checks of `commit`, under `label`, with each failing
/// one's annotations; returns whether all of them passed.
fn describe(label: &str, commit: &Commit, out: &mut String) -> bool {
    let runs: Vec<(String, &CheckRun)> = commit
        .check_suites
        .nodes
        .iter()
        .flat_map(|suite| {
            suite.check_runs.nodes.iter().map(move |run| {
                let name = suite.workflow_run.as_ref().map_or_else(
                    || run.name.clone(),
                    |workflow| format!("{} / {}", workflow.workflow.name, run.name),
                );
                (name, run)
            })
        })
        .collect();
    let count = |state: State| runs.iter().filter(|(_, run)| run.state() == state).count();
    let (failing, cancelled, running, passed) = (
        count(State::Failing),
        count(State::Cancelled),
        count(State::Running),
        count(State::Passed),
    );
    say(
        out,
        &format!(
            "{label} at {}: {failing} failing, {cancelled} cancelled, {running} running, {passed} \
         passed",
            commit.oid.get(..12).unwrap_or(&commit.oid)
        ),
    );
    for (name, run) in &runs {
        let state = match run.state() {
            State::Failing => "failing",
            State::Cancelled => "cancelled",
            State::Running => "running",
            State::Passed => continue,
        };
        let outcome = run
            .conclusion
            .as_deref()
            .unwrap_or(&run.status)
            .to_ascii_lowercase();
        say(
            out,
            &format!(
                "  {state:<9} {name}: {outcome} {}",
                run.details_url.as_deref().unwrap_or_default()
            ),
        );
        if run.state() != State::Failing {
            continue;
        }
        for note in &run.annotations.nodes {
            let lines = if note.location.start.line == note.location.end.line {
                note.location.start.line.to_string()
            } else {
                format!("{}-{}", note.location.start.line, note.location.end.line)
            };
            let level = note
                .annotation_level
                .as_deref()
                .unwrap_or("NOTICE")
                .to_ascii_lowercase();
            say(
                out,
                &format!(
                    "    {level} {}:{lines} {}: {}",
                    note.path,
                    note.title.as_deref().unwrap_or_default(),
                    note.message.replace('\n', "\n      ")
                ),
            );
        }
    }
    failing == 0 && cancelled == 0 && running == 0
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{latest_queue_commit, query, run};
    use crate::github::{Repo, Request};

    /// A check run as GitHub's GraphQL API answers it.
    fn check_run(name: &str, status: &str, conclusion: Option<&str>, notes: &Value) -> Value {
        json!({
            "name": name, "status": status, "conclusion": conclusion,
            "detailsUrl": format!("https://github.com/o/r/actions/runs/1/job/{name}"),
            "annotations": {"nodes": notes}
        })
    }

    /// A recorded GraphQL answer: the head with a failing Coverage run that has two annotations,
    /// a cancelled run, a running one and a passing one; and, when given, a merge-queue commit
    /// whose Mutants run failed.
    fn recorded(queue: bool) -> Value {
        let head = json!({
            "oid": "1111111111111111111111111111111111111111",
            "checkSuites": {"nodes": [
                {"workflowRun": {"workflow": {"name": "CI"}}, "checkRuns": {"nodes": [
                    check_run("Coverage", "COMPLETED", Some("FAILURE"), &json!([
                        {"path": "xtask/src/a.rs", "title": "Not covered", "message": "Lines 3-4 changed and no test runs them.", "annotationLevel": "FAILURE", "location": {"start": {"line": 3}, "end": {"line": 4}}},
                        {"path": ".github", "title": null, "message": "Process completed with exit code 1.\nSee the log.", "annotationLevel": "FAILURE", "location": {"start": {"line": 1}, "end": {"line": 1}}}
                    ])),
                    check_run("Fuzz", "COMPLETED", Some("CANCELLED"), &json!([
                        {"path": ".github", "title": null, "message": "The operation was canceled.", "annotationLevel": "FAILURE", "location": {"start": {"line": 1}, "end": {"line": 1}}}
                    ])),
                    check_run("Mutants", "IN_PROGRESS", None, &json!([])),
                    check_run("Check", "COMPLETED", Some("SUCCESS"), &json!([]))
                ]}},
                {"workflowRun": null, "checkRuns": {"nodes": [
                    check_run("Renovate", "COMPLETED", Some("SKIPPED"), &json!([]))
                ]}}
            ]}
        });
        let mut repository = json!({"pullRequest": {"commits": {"nodes": [{"commit": head}]}}});
        if queue {
            repository["queue"] = json!({
                "oid": "2222222222222222222222222222222222222222",
                "checkSuites": {"nodes": [{"workflowRun": {"workflow": {"name": "CI"}}, "checkRuns": {"nodes": [
                    check_run("Mutants (3/8)", "COMPLETED", Some("TIMED_OUT"), &json!([
                        {"path": "src/b.rs", "title": "Missed mutant", "message": "No test fails when the code is mutated: replace f with ().", "annotationLevel": "FAILURE", "location": {"start": {"line": 9}, "end": {"line": 9}}}
                    ]))
                ]}}]}
            });
        }
        json!({"data": {"repository": repository}})
    }

    /// Recorded merge-queue runs, newest first: another pull request's, then two of #25's.
    fn queue_runs() -> Value {
        json!({"workflow_runs": [
            {"head_branch": "gh-readonly-queue/main/pr-125-aaaa", "head_sha": "aaaa"},
            {"head_branch": "gh-readonly-queue/main/pr-25-2222", "head_sha": "2222222222222222222222222222222222222222"},
            {"head_branch": "gh-readonly-queue/main/pr-25-3333", "head_sha": "3333"}
        ]})
    }

    fn repo() -> Repo {
        Repo {
            owner: "o".to_owned(),
            name: "r".to_owned(),
        }
    }

    #[test]
    fn finds_the_latest_merge_queue_commit_of_the_pull_request() {
        assert_eq!(
            latest_queue_commit(&queue_runs(), 25),
            Some((
                "2222222222222222222222222222222222222222".to_owned(),
                "gh-readonly-queue/main/pr-25-2222".to_owned()
            )),
            "pr-125 is not pr-25, and the newest comes first"
        );
        assert_eq!(latest_queue_commit(&queue_runs(), 7), None);
        assert_eq!(latest_queue_commit(&json!({}), 25), None);
        assert!(query(true).contains("queue: object(oid: $queue) { ...runs }"));
        assert!(query(true).contains("$queue: GitObjectID!"));
        assert!(!query(false).contains("$queue"));
    }

    #[test]
    fn lists_what_fails_with_its_annotations_in_two_calls() {
        let mut requests: Vec<Request> = Vec::new();
        let mut answers = vec![queue_runs(), recorded(true)].into_iter();
        let mut send = |request: &Request| {
            requests.push(request.clone());
            Ok(answers.next().unwrap())
        };
        let mut out = String::new();
        assert!(!run(&repo(), 25, &mut send, &mut out).unwrap());
        assert_eq!(
            out,
            "#25 head at 111111111111: 1 failing, 1 cancelled, 1 running, 2 passed\n  \
             failing   CI / Coverage: failure \
             https://github.com/o/r/actions/runs/1/job/Coverage\n    failure xtask/src/a.rs:3-4 \
             Not covered: Lines 3-4 changed and no test runs them.\n    failure .github:1 : \
             Process completed with exit code 1.\n      See the log.\n  cancelled CI / Fuzz: \
             cancelled https://github.com/o/r/actions/runs/1/job/Fuzz\n  running   CI / \
             Mutants: in_progress https://github.com/o/r/actions/runs/1/job/Mutants\nmerge \
             queue gh-readonly-queue/main/pr-25-2222 at 222222222222: 1 failing, 0 cancelled, 0 \
             running, 0 passed\n  failing   CI / Mutants (3/8): timed_out \
             https://github.com/o/r/actions/runs/1/job/Mutants (3/8)\n    failure src/b.rs:9 \
             Missed mutant: No test fails when the code is mutated: replace f with ().\n"
        );
        assert_eq!(requests.len(), 2, "one REST call and one GraphQL query");
        assert_eq!(
            requests[0].path,
            "/repos/o/r/actions/runs?event=merge_group&per_page=100"
        );
        let body = requests[1].body.as_ref().unwrap();
        assert_eq!(
            body["variables"],
            json!({"owner": "o", "name": "r", "number": 25, "queue": "2222222222222222222222222222222222222222"})
        );
    }

    #[test]
    fn a_failing_merge_queue_run_fails_a_clean_head() {
        let mut answers = vec![queue_runs(), {
            let mut clean = recorded(true);
            let runs = clean
                .pointer_mut(
                    "/data/repository/pullRequest/commits/nodes/0/commit/checkSuites/nodes/0/checkRuns/nodes",
                )
                .unwrap();
            *runs = json!([check_run("Check", "COMPLETED", Some("SUCCESS"), &json!([]))]);
            clean
        }]
        .into_iter();
        let mut send = |_: &Request| Ok(answers.next().unwrap());
        let mut out = String::new();
        assert!(!run(&repo(), 25, &mut send, &mut out).unwrap(), "{out}");
        assert!(out.contains("0 failing, 0 cancelled, 0 running"), "{out}");
    }

    #[test]
    fn only_finished_passing_checks_pass() {
        let commit = |run: Value| -> super::Commit {
            serde_json::from_value(json!({
                "oid": "3333333333333333333333333333333333333333",
                "checkSuites": {"nodes": [{"workflowRun": null, "checkRuns": {"nodes": [run]}}]}
            }))
            .unwrap()
        };
        for (status, conclusion, passes) in [
            ("COMPLETED", Some("SUCCESS"), true),
            ("COMPLETED", Some("CANCELLED"), false),
            ("IN_PROGRESS", None, false),
            ("COMPLETED", Some("FAILURE"), false),
        ] {
            let mut out = String::new();
            assert_eq!(
                super::describe(
                    "x",
                    &commit(check_run("C", status, conclusion, &json!([]))),
                    &mut out
                ),
                passes,
                "{status} {conclusion:?}: {out}"
            );
        }
    }

    #[test]
    fn a_clean_head_without_a_merge_queue_run_passes() {
        let mut answers = vec![json!({"workflow_runs": []}), {
            let mut clean = recorded(false);
            let runs = clean
                .pointer_mut(
                    "/data/repository/pullRequest/commits/nodes/0/commit/checkSuites/nodes/0/checkRuns/nodes",
                )
                .unwrap();
            *runs = json!([check_run("Check", "COMPLETED", Some("SUCCESS"), &json!([]))]);
            clean
        }]
        .into_iter();
        let mut send = |_: &Request| Ok(answers.next().unwrap());
        let mut out = String::new();
        assert!(run(&repo(), 7, &mut send, &mut out).unwrap(), "{out}");
        assert_eq!(
            out,
            "#7 head at 111111111111: 0 failing, 0 cancelled, 0 running, 2 passed\nmerge queue: no \
             runs for this pull request\n"
        );
        let mut missing = vec![
            json!({"workflow_runs": []}),
            json!({"data": {"repository": {"pullRequest": null}}}),
        ]
        .into_iter();
        let mut absent = |_: &Request| Ok(missing.next().unwrap());
        let error = format!("{:#}", run(&repo(), 9, &mut absent, &mut out).unwrap_err());
        assert_eq!(error, "GitHub has no pull request #9 with commits");
    }
}
