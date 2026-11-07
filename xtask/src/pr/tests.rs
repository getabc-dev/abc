use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{APP_LOGIN, Action, Plan, prepare, send};
use crate::github::{Repo, Request};
use crate::review::MAX_DIFF_BYTES;
use crate::testing::{clone_with_origin, git};

/// A checkout whose `origin/main` has a test in `src/t.rs`, on branch `agent/x`, which deletes
/// it, pushed.
fn checkout(temp: &Path) -> PathBuf {
    let work = clone_with_origin(temp);
    fs::create_dir_all(work.join("src")).unwrap();
    fs::write(work.join("src/t.rs"), "#[test]\nfn kept() {}\n").unwrap();
    git(&work, &["add", "--all"]);
    git(&work, &["commit", "--quiet", "-m", "a test"]);
    git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    git(&work, &["fetch", "--quiet", "origin"]);
    git(&work, &["checkout", "--quiet", "-b", "agent/x"]);
    fs::remove_file(work.join("src/t.rs")).unwrap();
    git(
        &work,
        &["commit", "--quiet", "--all", "-m", "drop the test"],
    );
    git(&work, &["push", "--quiet", "origin", "agent/x"]);
    work
}

const REASONED: &str = "Drops t.\n\n## Test changes\n\n- `src/t.rs`: the parser it covered is \
                        gone.\n\n## Review findings\n\nNone: `/code-review high` and \
                        `/security-review` found nothing.\n";

/// The description `text`, in a file under `dir`.
fn body(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("body.md");
    fs::write(&path, text).unwrap();
    path
}

fn open(title: &str, body: PathBuf) -> Action {
    Action::Open {
        title: title.to_owned(),
        body,
    }
}

fn refusal(action: Action, root: &Path) -> String {
    format!(
        "{:#}",
        prepare(action, root, &mut String::new()).unwrap_err()
    )
}

fn repo() -> Repo {
    Repo {
        owner: "o".to_owned(),
        name: "r".to_owned(),
    }
}

#[test]
fn prints_the_test_changes_section_to_fill_in() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let mut out = String::new();
    assert_eq!(prepare(Action::TestChanges, &work, &mut out).unwrap(), None);
    assert_eq!(
        out,
        "## Test changes\n\n- `src/t.rs`: <which requirement or behaviour changed>\n"
    );
}

#[test]
fn opens_only_what_integrity_the_review_and_the_rulesets_would_pass() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let unexplained = refusal(open("t", body(temp.path(), "Drops t.\n")), &work);
    assert!(
        unexplained.contains("- `src/t.rs`: <which requirement")
            && unexplained.ends_with("`cargo xtask pr test-changes` prints the section to fill in"),
        "#21 missed an entry: {unexplained}"
    );
    let stale = refusal(
        open(
            "t",
            body(
                temp.path(),
                &REASONED.replacen(
                    "gone.\n",
                    "gone.\n- `src/u.rs::tests::gone`: from an earlier push.\n",
                    1,
                ),
            ),
        ),
        &work,
    );
    assert!(
        stale.contains("names `src/u.rs::tests::gone`, which has no flagged change"),
        "#25 kept three stale entries: {stale}"
    );
    // #33: the B1 reviews found a data-loss bug and 8 more issues every gate passed. They run
    // before the pull request opens, and its description says what they found.
    let unreviewed_text = REASONED.split("## Review findings").next().unwrap();
    let unreviewed = refusal(open("t", body(temp.path(), unreviewed_text)), &work);
    assert!(
        unreviewed.contains("## Review findings")
            && unreviewed.contains("/code-review high")
            && unreviewed.contains("/security-review"),
        "{unreviewed}"
    );
    let blank = refusal(
        open(
            "t",
            body(
                temp.path(),
                &format!(
                    "{unreviewed_text}## Review findings\n\n<!-- what they found -->\n\n## After\n\nx\n"
                ),
            ),
        ),
        &work,
    );
    assert!(blank.contains("## Review findings"), "{blank}");
    let empty = refusal(open("t", body(temp.path(), "\n")), &work);
    assert!(
        empty.ends_with("is empty; a pull request needs a description"),
        "{empty}"
    );
    let oversized = refusal(
        open(&"t".repeat(MAX_DIFF_BYTES), body(temp.path(), REASONED)),
        &work,
    );
    assert!(
        oversized.starts_with("the diff with the title and description is "),
        "the title counts: {oversized}"
    );

    let mut out = String::new();
    let plan = prepare(
        open("feat: drop t", body(temp.path(), REASONED)),
        &work,
        &mut out,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        plan,
        Plan::Open {
            title: "feat: drop t".to_owned(),
            body: REASONED.to_owned(),
            branch: "agent/x".to_owned(),
            base: "main".to_owned(),
        }
    );
    assert_eq!(out, "", "nothing to say before sending");

    git(
        &work,
        &["commit", "--quiet", "--allow-empty", "-m", "not pushed"],
    );
    let unpushed = refusal(open("t", body(temp.path(), REASONED)), &work);
    assert!(
        unpushed.contains("push it first (`git push origin agent/x`)"),
        "{unpushed}"
    );
    fs::write(work.join("loose.txt"), "x\n").unwrap();
    let dirty = refusal(open("t", body(temp.path(), REASONED)), &work);
    assert!(dirty.contains("commit them first"), "{dirty}");
    fs::remove_file(work.join("loose.txt")).unwrap();
    fs::create_dir_all(work.join(".github/workflows")).unwrap();
    fs::write(work.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    git(&work, &["add", "--all"]);
    git(&work, &["commit", "--quiet", "-m", "ci"]);
    let rulesets = refusal(open("t", body(temp.path(), REASONED)), &work);
    assert!(
        rulesets.starts_with(".github/ changed and `rulesets check` fails"),
        "{rulesets}"
    );
    git(&work, &["checkout", "--quiet", "--detach"]);
    let detached = refusal(open("t", body(temp.path(), REASONED)), &work);
    assert!(detached.starts_with("HEAD is on no branch"), "{detached}");
}

#[test]
fn opens_through_the_rest_api() {
    let temp = tempfile::tempdir().unwrap();
    let mut sent = Vec::new();
    let mut reply = |request: &Request| {
        sent.push(request.clone());
        Ok(json!({"number": 31, "html_url": "https://github.com/o/r/pull/31"}))
    };
    let mut out = String::new();
    let plan = Plan::Open {
        title: "feat: x".to_owned(),
        body: "B".to_owned(),
        branch: "agent/x".to_owned(),
        base: "main".to_owned(),
    };
    assert!(
        send(
            plan,
            temp.path(),
            &repo(),
            &mut reply,
            &mut no_downloads,
            &mut out
        )
        .unwrap()
    );
    assert_eq!(out, "pr: opened #31 https://github.com/o/r/pull/31\n");
    assert_eq!(
        sent,
        [Request {
            method: "POST",
            path: "/repos/o/r/pulls".to_owned(),
            body: Some(json!({"title": "feat: x", "head": "agent/x", "base": "main", "body": "B"})),
        }]
    );
}

/// A `ci.yml` run for `head` on this repository's `agent/x`, started by `actor` at `at`, as
/// GitHub records it.
fn ci_run(head: &str, actor: &str, at: &str) -> Value {
    run_from(head, "o/r", "agent/x", actor, at)
}

/// A `ci.yml` run for `head` on `repository`'s `branch`, started by `actor` at `at`.
fn run_from(head: &str, repository: &str, branch: &str, actor: &str, at: &str) -> Value {
    json!({
        "path": ".github/workflows/ci.yml",
        "event": "pull_request",
        "head_sha": head,
        "head_branch": branch,
        "head_repository": {"full_name": repository},
        "pull_requests": [],
        "actor": {"login": actor},
        "triggering_actor": {"login": "someone"},
        "created_at": at,
    })
}

/// GitHub's recorded replies to `pr update` of #7, by path: the pull request at `head` from
/// `agent/x`, `runs` as its `ci.yml` runs (and, for the first version's query, all its runs),
/// the timeline `events`, and the edited pull request.
fn github(head: &str, runs: &[Value], events: &[Value]) -> impl FnMut(&Request) -> Value + use<> {
    let head = head.to_owned();
    let runs = runs.to_vec();
    let events = events.to_vec();
    move |request| {
        let path = request.path.as_str();
        if request.method == "PATCH" {
            json!({"number": 7})
        } else if path == "/repos/o/r/pulls/7" {
            json!({"title": "feat: drop t", "head": {"ref": "agent/x", "sha": head, "repo": {"full_name": "o/r"}}})
        } else if path.starts_with("/repos/o/r/actions/") && path.ends_with("&page=1") {
            json!({"total_count": runs.len(), "workflow_runs": runs})
        } else if path.starts_with("/repos/o/r/actions/") {
            json!({"workflow_runs": []})
        } else if path == "/repos/o/r/issues/7/timeline?per_page=100&page=1" {
            Value::Array(events.clone())
        } else {
            json!([])
        }
    }
}

/// GitHub's recorded replies to `pr update`: the pull request at `head`, the CI run of its push
/// by `pusher` (no run when empty), and the edited pull request.
fn replies(head: &str, pusher: &str) -> impl FnMut(&Request) -> Value + use<> {
    let runs = if pusher.is_empty() {
        Vec::new()
    } else {
        vec![ci_run(head, pusher, "2026-11-05T10:00:00Z")]
    };
    github(head, &runs, &[])
}

/// Runs `pr update 7` on `work` against `answer`; its result, output and requests.
fn update(
    work: &Path,
    text: &str,
    mut answer: impl FnMut(&Request) -> Value,
) -> (anyhow::Result<bool>, String, Vec<Request>) {
    let plan = prepare(
        Action::Update {
            number: 7,
            body: body(work.parent().unwrap(), text),
        },
        work,
        &mut String::new(),
    )
    .unwrap()
    .unwrap();
    let mut sent = Vec::new();
    let mut reply = |request: &Request| {
        sent.push(request.clone());
        Ok(answer(request))
    };
    let mut out = String::new();
    let result = send(plan, work, &repo(), &mut reply, &mut no_downloads, &mut out);
    (result, out, sent)
}

#[test]
fn updates_the_description_when_the_app_pushed_last() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let (result, out, sent) = update(&work, REASONED, replies(&head, APP_LOGIN));
    assert!(result.unwrap(), "{out}");
    assert_eq!(
        out,
        "pr: updated the description of #7\npr: the latest push to #7 is the agent App's; the \
         hand-over is done\n"
    );
    assert_eq!(sent[0].path, "/repos/o/r/pulls/7");
    assert_eq!(
        sent[1].path,
        "/repos/o/r/issues/7/timeline?per_page=100&page=1"
    );
    assert_eq!(
        sent[2].path,
        format!(
            "/repos/o/r/actions/workflows/ci.yml/runs?event=pull_request&head_sha={head}&per_page=100&page=1"
        )
    );
    assert_eq!(
        sent[3],
        Request {
            method: "PATCH",
            path: "/repos/o/r/pulls/7".to_owned(),
            body: Some(json!({"body": REASONED})),
        },
        "through the REST API, never gh pr edit"
    );
    assert_eq!(sent.len(), 4);
}

#[test]
fn after_the_maintainers_push_only_a_push_that_changes_the_diff_hands_over() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let theirs = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let (same, _, refused) = update(&work, REASONED, replies(&theirs, "snehilvj"));
    let same = format!("{:#}", same.unwrap_err());
    assert!(same.contains("HEAD changes nothing in it"), "{same}");
    assert_eq!(refused.len(), 3, "refused before the description was sent");

    // #25 lost a round this way: an empty commit is a new commit but no reviewable push.
    git(
        &work,
        &["commit", "--quiet", "--allow-empty", "-m", "hand over"],
    );
    let (empty, _, _) = update(&work, REASONED, replies(&theirs, "snehilvj"));
    let empty = format!("{:#}", empty.unwrap_err());
    assert!(
        empty.contains("GitHub does not count a push that leaves the diff as it is"),
        "{empty}"
    );

    fs::write(work.join("note.txt"), "a real change\n").unwrap();
    git(&work, &["add", "note.txt"]);
    git(&work, &["commit", "--quiet", "-m", "a change"]);
    let (changed, out, sent) = update(&work, REASONED, replies(&theirs, "snehilvj"));
    assert!(
        !changed.unwrap(),
        "not handed over until HEAD is pushed: {out}"
    );
    assert!(
        out.ends_with(
            "pr: snehilvj pushed #7's head last; push HEAD now: it changes the diff, so GitHub \
             counts it as a reviewable push and the maintainer can approve\n"
        ),
        "{out}"
    );
    assert_eq!(sent.len(), 4, "the description is sent");

    let unknown = "0123456789012345678901234567890123456789";
    let (missing, _, _) = update(&work, REASONED, replies(unknown, "snehilvj"));
    assert!(
        format!("{:#}", missing.unwrap_err()).contains("this checkout does not have it"),
        "the maintainer's commit must be fetched first"
    );
}

#[test]
fn an_update_refuses_what_it_cannot_judge() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let (no_run, _, _) = update(&work, REASONED, replies(&head, ""));
    assert!(
        format!("{:#}", no_run.unwrap_err()).contains("so who pushed it is unknown"),
        "no CI run, no pusher"
    );
    let (elsewhere, _, _) = update(
        &work,
        REASONED,
        |_: &Request| json!({"title": "t", "head": {"ref": "agent/y", "sha": head}}),
    );
    assert_eq!(
        format!("{:#}", elsewhere.unwrap_err()),
        "#7 comes from agent/y, but HEAD is on agent/x"
    );
    let long = "x".repeat(MAX_DIFF_BYTES);
    let (oversized, _, _) = update(
        &work,
        REASONED,
        |_: &Request| json!({"title": long, "head": {"ref": "agent/x", "sha": head}}),
    );
    assert!(
        format!("{:#}", oversized.unwrap_err())
            .starts_with("the diff with the title and description is "),
        "the pull request's title counts"
    );
}

/// #32: the maintainer's `integrity-approved` label started new runs for the same head, newest
/// first in GitHub's list, so `pr update` read their actor and named the maintainer as the
/// pusher of the App's push. Only `ci.yml`'s runs count, which no label starts, across pages.
#[test]
fn a_label_re_run_does_not_hide_who_pushed() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    // A full first page of runs for the same commit from other branches, then the push.
    let elsewhere: Vec<Value> = (0..100)
        .map(|minute| {
            run_from(
                &head,
                "o/r",
                "agent/y",
                "snehilvj",
                &format!("2026-11-05T12:{:02}:00Z", 59 - minute % 60),
            )
        })
        .collect();
    let mut answer = github(&head, &elsewhere, &[]);
    let runs = format!(
        "/repos/o/r/actions/workflows/ci.yml/runs?event=pull_request&head_sha={head}&per_page=100&page=2"
    );
    let push = ci_run(&head, APP_LOGIN, "2026-11-05T10:00:00Z");
    let (result, out, sent) = update(&work, REASONED, move |request: &Request| {
        if request.path == runs {
            json!({"workflow_runs": [push.clone()]})
        } else {
            answer(request)
        }
    });
    assert!(result.unwrap(), "the App pushed the head: {out}");
    assert!(out.contains("is the agent App's"), "{out}");
    assert!(
        sent.iter().any(|request| request.path.ends_with("&page=2")),
        "{sent:?}"
    );
}

/// The earliest run for a head was taken as its push, but the same commit can head another pull
/// request, from a fork: its earlier run named that fork's author as this one's pusher.
#[test]
fn a_run_for_the_same_commit_elsewhere_is_not_this_pull_requests_push() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let runs = [
        ci_run(&head, APP_LOGIN, "2026-11-05T10:00:00Z"),
        run_from(
            &head,
            "someone/abc",
            "agent/x",
            "someone",
            "2026-11-05T09:00:00Z",
        ),
    ];
    let (result, out, _) = update(&work, REASONED, github(&head, &runs, &[]));
    assert!(
        result.unwrap(),
        "the App pushed this pull request's head: {out}"
    );
}

/// A push that started no CI run left only the runs a label started for its head, and the first
/// of them was taken as the push, naming the labeller as the pusher.
#[test]
fn a_push_that_started_no_run_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let label_run = json!({
        "path": ".github/workflows/integrity.yml",
        "event": "pull_request",
        "head_sha": head,
        "head_branch": "agent/x",
        "head_repository": {"full_name": "o/r"},
        "actor": {"login": "snehilvj"},
        "created_at": "2026-11-05T11:00:00Z",
    });
    let mut answer = github(&head, &[], &[]);
    let (result, _, _) = update(&work, REASONED, move |request: &Request| {
        if request.path.starts_with("/repos/o/r/actions/runs?") {
            json!({"workflow_runs": [label_run.clone()]})
        } else {
            answer(request)
        }
    });
    assert!(
        format!("{:#}", result.unwrap_err()).contains("so who pushed it is unknown"),
        "a label's run is no push"
    );
}

/// A force push that brings back an earlier commit is its latest push, though the commit's first
/// run is older.
#[test]
fn a_force_push_back_is_the_latest_push() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let runs = [ci_run(&head, APP_LOGIN, "2026-11-05T09:00:00Z")];
    let events = [json!({
        "event": "head_ref_force_pushed",
        "commit_id": head,
        "actor": {"login": "snehilvj"},
        "created_at": "2026-11-05T10:30:00Z",
    })];
    let (result, _, _) = update(&work, REASONED, github(&head, &runs, &events));
    assert!(
        format!("{:#}", result.unwrap_err()).contains("snehilvj pushed #7's head last"),
        "the maintainer's force push is the latest"
    );
}

/// #33 and #38 were built at once; #33 broke on its merge with main once #38 merged. `pr open`
/// and `pr update` fetch main and warn when it moved on files the branch changes too.
#[test]
fn warns_when_main_moved_on_the_branchs_files() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let prepared = |action: Action| {
        let mut out = String::new();
        prepare(action, &work, &mut out).unwrap().unwrap();
        out
    };
    assert_eq!(
        prepared(open("feat: x", body(temp.path(), REASONED))),
        "",
        "main has not moved"
    );
    // Another branch merges into main: it changes src/t.rs, which agent/x deletes, and a file
    // agent/x leaves alone. This checkout has not fetched it.
    let other = temp.path().join("other");
    git(
        temp.path(),
        &[
            "clone",
            "--quiet",
            temp.path().join("origin.git").to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    fs::write(
        other.join("src/t.rs"),
        "#[test]\nfn kept() { assert!(true); }\n",
    )
    .unwrap();
    fs::write(other.join("README.md"), "moved\n").unwrap();
    git(&other, &["add", "--all"]);
    git(
        &other,
        &[
            "-c",
            "user.name=other",
            "-c",
            "user.email=other@example.com",
            "commit",
            "--quiet",
            "-m",
            "#38",
        ],
    );
    git(&other, &["push", "--quiet", "origin", "HEAD:main"]);
    let warned = "pr: warning: origin/main moved on since this branch left it, changing 1 \
                  file(s) this branch changes too:\n  src/t.rs\npr: merge it (`git merge \
                  origin/main`), check again and push; never rebase a pushed branch\n";
    assert_eq!(
        prepared(open("feat: x", body(temp.path(), REASONED))),
        warned
    );
    assert_eq!(
        prepared(Action::Update {
            number: 7,
            body: body(temp.path(), REASONED),
        }),
        warned
    );
    git(&work, &["remote", "set-url", "origin", "/no/such/remote"]);
    let offline = prepared(Action::Update {
        number: 7,
        body: body(temp.path(), REASONED),
    });
    assert!(
        offline.starts_with("pr: could not fetch origin/main, so it is compared as last fetched")
            && offline.ends_with(warned),
        "{offline}"
    );
}

/// `pr open` and `pr update` fetch the default branch. On an HTTPS remote with no credential
/// helper, git asked for a username on the terminal and waited; it now fails at once, and the
/// branch is compared as last fetched.
#[test]
fn a_remote_that_asks_for_credentials_never_waits() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let asked = temp.path().join("asked");
    crate::testing::script(
        &temp.path().join("askpass"),
        &format!("touch {}\necho secret\n", asked.display()),
    );
    git(
        &work,
        &[
            "config",
            "core.askPass",
            temp.path().join("askpass").to_str().unwrap(),
        ],
    );
    let (url, server) = crate::testing::serve(vec![("401 Unauthorized", "")]);
    git(
        &work,
        &["remote", "set-url", "origin", &format!("{url}/abc.git")],
    );
    let mut out = String::new();
    prepare(
        Action::Update {
            number: 7,
            body: body(temp.path(), REASONED),
        },
        &work,
        &mut out,
    )
    .unwrap()
    .unwrap();
    assert!(
        out.starts_with("pr: could not fetch origin/main, so it is compared as last fetched")
            && out.contains("terminal prompts disabled"),
        "{out}"
    );
    assert!(!asked.exists(), "nothing asked for credentials");
    server.join().unwrap();
}

/// Two of the head's own runs in the same second: GitHub lists the newer first, and the first
/// read stays.
#[test]
fn of_two_runs_in_one_second_the_first_listed_counts() {
    let runs = [
        ci_run("abc", APP_LOGIN, "2026-11-05T10:00:00Z"),
        ci_run("abc", "snehilvj", "2026-11-05T10:00:00Z"),
    ];
    let mut answers = vec![json!({"workflow_runs": runs})].into_iter();
    let push = crate::github::push_of(
        crate::github::Head {
            sha: "abc",
            repository: "o/r",
            branch: "agent/x",
        },
        &[],
        &mut |_| Ok(answers.next().unwrap()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(push.pusher, APP_LOGIN);
}

/// #33 broke once #38 merged, and nothing said so before the merge. `pr checks` warns when the
/// default branch moved, since the pull request's branch left it, on files the pull request
/// changes, from GitHub's compare API.
#[test]
fn checks_warn_when_main_moved_on_the_pull_requests_files() {
    let answer = |behind: u64, state: &'static str| {
        move |request: &Request| -> Value {
            match (request.method, request.path.as_str()) {
                ("GET", "/repos/o/r/pulls/25") => {
                    json!({"state": state, "base": {"ref": "main"}, "head": {"sha": "abc"}})
                }
                ("GET", "/repos/o/r/compare/abc...main") => json!({
                    "ahead_by": behind,
                    "files": [{"filename": "b.rs"}, {"filename": "c.rs", "previous_filename": "a.rs"}]
                }),
                ("GET", "/repos/o/r/compare/main...abc") => json!({
                    "ahead_by": 3,
                    "files": [{"filename": "a.rs"}, {"filename": "b.rs"}, {"filename": "d.rs"}]
                }),
                ("GET", path) if path.starts_with("/repos/o/r/actions/runs") => {
                    json!({"workflow_runs": []})
                }
                ("POST", "/graphql") => {
                    json!({"data": {"repository": {"pullRequest": {"commits": {"nodes": [
                        {"commit": {"oid": "1111111111111111111111111111111111111111", "checkSuites": {"nodes": []}}}
                    ]}}}}})
                }
                other => panic!("unexpected call {other:?}"),
            }
        }
    };
    let run = |behind: u64, state: &'static str| {
        let reply = answer(behind, state);
        let mut send = |request: &Request| Ok(reply(request));
        let mut out = String::new();
        let passed = send_plan(Plan::Checks(25), &mut send, &mut out);
        (passed, out)
    };
    let (passed, out) = run(2, "open");
    assert!(passed, "a warning, not a failure: {out}");
    assert!(
        out.starts_with(
            "pr: warning: main moved on by 2 commit(s) since #25's branch left it, changing 2 \
             file(s) #25 changes too:\n  a.rs\n  b.rs\npr: merge it into the branch (`git merge \
             origin/main`), check again and push before #25 merges; never rebase a pushed \
             branch\n#25 head at 111111111111"
        ),
        "{out}"
    );
    let (_, current) = run(0, "open");
    assert!(current.starts_with("#25 head at"), "not behind: {current}");
    let (_, merged) = run(2, "closed");
    assert!(merged.starts_with("#25 head at"), "closed: {merged}");
}

/// Carries out `plan` for `o/r` through `send` in a checkout that is not used.
fn send_plan(
    plan: Plan,
    send: &mut impl FnMut(&Request) -> anyhow::Result<Value>,
    out: &mut String,
) -> bool {
    let temp = tempfile::tempdir().unwrap();
    super::send(plan, temp.path(), &repo(), send, &mut no_downloads, out).unwrap()
}

/// A download where none is expected.
fn no_downloads(url: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("no download expected: {url}")
}
