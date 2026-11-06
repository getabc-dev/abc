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

const REASONED: &str =
    "Drops t.\n\n## Test changes\n\n- `src/t.rs`: the parser it covered is gone.\n";

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
                &format!("{REASONED}- `src/u.rs::tests::gone`: from an earlier push.\n"),
            ),
        ),
        &work,
    );
    assert!(
        stale.contains("names `src/u.rs::tests::gone`, which has no flagged change"),
        "#25 kept three stale entries: {stale}"
    );
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
    assert!(send(plan, temp.path(), &repo(), &mut reply, &mut out).unwrap());
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

/// GitHub's recorded replies to `pr update`: the pull request at `head`, the CI run of its push
/// by `pusher` (no run when empty), and the edited pull request.
fn replies(head: &str, pusher: &str) -> std::vec::IntoIter<Value> {
    let runs = if pusher.is_empty() {
        json!({"total_count": 0, "workflow_runs": []})
    } else {
        json!({"workflow_runs": [{"actor": {"login": pusher}, "triggering_actor": {"login": "someone"}, "created_at": "2026-11-05T10:00:00Z"}]})
    };
    vec![
        json!({"title": "feat: drop t", "head": {"ref": "agent/x", "sha": head}}),
        runs,
        json!({"number": 7}),
    ]
    .into_iter()
}

/// Runs `pr update 7` on `work` against `answers`; its result, output and requests.
fn update(
    work: &Path,
    text: &str,
    answers: std::vec::IntoIter<Value>,
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
    let mut answers = answers;
    let mut sent = Vec::new();
    let mut reply = |request: &Request| {
        sent.push(request.clone());
        Ok(answers.next().unwrap())
    };
    let mut out = String::new();
    let result = send(plan, work, &repo(), &mut reply, &mut out);
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
        format!("/repos/o/r/actions/runs?event=pull_request&head_sha={head}&per_page=100&page=1")
    );
    assert_eq!(
        sent[2],
        Request {
            method: "PATCH",
            path: "/repos/o/r/pulls/7".to_owned(),
            body: Some(json!({"body": REASONED})),
        },
        "through the REST API, never gh pr edit"
    );
    assert_eq!(sent.len(), 3);
}

#[test]
fn after_the_maintainers_push_only_a_push_that_changes_the_diff_hands_over() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let theirs = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let (same, _, refused) = update(&work, REASONED, replies(&theirs, "snehilvj"));
    let same = format!("{:#}", same.unwrap_err());
    assert!(same.contains("HEAD changes nothing in it"), "{same}");
    assert_eq!(refused.len(), 2, "refused before the description was sent");

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
    assert_eq!(sent.len(), 3, "the description is sent");

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
    let other = vec![json!({"title": "t", "head": {"ref": "agent/y", "sha": head}})].into_iter();
    let (elsewhere, _, _) = update(&work, REASONED, other);
    assert_eq!(
        format!("{:#}", elsewhere.unwrap_err()),
        "#7 comes from agent/y, but HEAD is on agent/x"
    );
    let long = "x".repeat(MAX_DIFF_BYTES);
    let titled = vec![json!({"title": long, "head": {"ref": "agent/x", "sha": head}})].into_iter();
    let (oversized, _, _) = update(&work, REASONED, titled);
    assert!(
        format!("{:#}", oversized.unwrap_err())
            .starts_with("the diff with the title and description is "),
        "the pull request's title counts"
    );
}

/// #32: the maintainer's `integrity-approved` label started a new run for the same head, newest
/// first in GitHub's list, so `pr update` read its actor and named the maintainer as the pusher
/// of the App's push. The push is the earliest run, across pages.
#[test]
fn a_label_re_run_does_not_hide_who_pushed() {
    let temp = tempfile::tempdir().unwrap();
    let work = checkout(temp.path());
    let head = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
    let run = |login: &str, at: &str| json!({"actor": {"login": login}, "created_at": at});
    // A full first page of later runs, newest first, then the push on the second page.
    let mut newest: Vec<Value> = (0..100)
        .map(|minute| {
            run(
                "snehilvj",
                &format!("2026-11-05T12:{:02}:00Z", 59 - minute % 60),
            )
        })
        .collect();
    newest[0] = run("snehilvj", "2026-11-05T13:00:00Z");
    let answers = vec![
        json!({"title": "feat: drop t", "head": {"ref": "agent/x", "sha": head}}),
        json!({"workflow_runs": newest}),
        json!({"workflow_runs": [run(APP_LOGIN, "2026-11-05T10:00:00Z")]}),
        json!({"number": 7}),
    ];
    let (result, out, sent) = update(&work, REASONED, answers.into_iter());
    assert!(result.unwrap(), "the App pushed the head: {out}");
    assert!(out.contains("is the agent App's"), "{out}");
    assert_eq!(
        sent[2].path,
        format!("/repos/o/r/actions/runs?event=pull_request&head_sha={head}&per_page=100&page=2")
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
