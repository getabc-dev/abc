//! Branch rules for `main` as code, in `.github/rulesets/main.json`.
//!
//! `cargo xtask rulesets check` keeps the ruleset in step with CI: its required status checks
//! must be exactly the jobs of the workflows that run on both pull requests and the merge queue,
//! since only those can report in both places, so renaming or adding a job fails the build.
//! `cargo xtask rulesets apply`, run by the user with their own gh login, puts the file on
//! GitHub in place of the rulesets it replaces.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use yaml_rust2::{Yaml, YamlLoader};

use crate::git;

/// The ruleset file, relative to the repository root.
pub(crate) const RULESET: &str = ".github/rulesets/main.json";

/// The rulesets this one replaces on GitHub.
const REPLACED: [&str; 2] = ["Protect main", "main: checks must pass"];

/// The parts of the ruleset the check reads.
#[derive(Deserialize)]
struct Ruleset {
    name: String,
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
struct Rule {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    parameters: serde_json::Value,
}

/// The status checks a ruleset must require: the job names of every workflow in `workflows`
/// that runs on both `pull_request` and `merge_group`.
fn required_checks(workflows: &[(String, String)]) -> anyhow::Result<BTreeSet<String>> {
    let mut checks = BTreeSet::new();
    for (file, text) in workflows {
        let documents =
            YamlLoader::load_from_str(text).with_context(|| format!("{file} is not YAML"))?;
        let Some(workflow) = documents.first() else {
            continue;
        };
        let triggers = triggers(&workflow["on"]);
        if !(triggers.contains("pull_request") && triggers.contains("merge_group")) {
            continue;
        }
        let jobs = workflow["jobs"]
            .as_hash()
            .with_context(|| format!("{file} has no jobs"))?;
        for (id, job) in jobs {
            let name = job["name"]
                .as_str()
                .or_else(|| id.as_str())
                .with_context(|| format!("{file} has a job without a name"))?;
            checks.insert(name.to_owned());
        }
    }
    Ok(checks)
}

/// The events in a workflow's `on`: a name, a list of names, or a map keyed by name.
fn triggers(on: &Yaml) -> BTreeSet<String> {
    match on {
        Yaml::String(event) => BTreeSet::from([event.clone()]),
        Yaml::Array(events) => events
            .iter()
            .filter_map(Yaml::as_str)
            .map(str::to_owned)
            .collect(),
        Yaml::Hash(events) => events
            .keys()
            .filter_map(Yaml::as_str)
            .map(str::to_owned)
            .collect(),
        Yaml::Real(_)
        | Yaml::Integer(_)
        | Yaml::Boolean(_)
        | Yaml::Alias(_)
        | Yaml::Null
        | Yaml::BadValue => BTreeSet::new(),
    }
}

/// Checks `ruleset` (the JSON text) against `workflows`, as file names and texts.
fn check_text(ruleset: &str, workflows: &[(String, String)]) -> anyhow::Result<()> {
    let ruleset: Ruleset = serde_json::from_str(ruleset).context("the ruleset is not valid")?;
    let rule = |kind: &str| ruleset.rules.iter().find(|rule| rule.kind == kind);
    for kind in [
        "deletion",
        "non_fast_forward",
        "pull_request",
        "merge_queue",
    ] {
        ensure!(rule(kind).is_some(), "the ruleset has no {kind} rule");
    }
    let review = &rule("pull_request").context("checked above")?.parameters;
    for (field, expected) in [
        ("required_approving_review_count", serde_json::json!(1)),
        ("require_code_owner_review", serde_json::json!(true)),
        ("dismiss_stale_reviews_on_push", serde_json::json!(true)),
        ("require_last_push_approval", serde_json::json!(true)),
    ] {
        ensure!(
            review.get(field) == Some(&expected),
            "the pull_request rule needs {field} = {expected}"
        );
    }
    ensure!(
        rule("merge_queue")
            .context("checked above")?
            .parameters
            .get("merge_method")
            .and_then(serde_json::Value::as_str)
            == Some("SQUASH"),
        "the merge queue must squash"
    );
    let required: BTreeSet<String> = rule("required_status_checks")
        .context("the ruleset has no required_status_checks rule")?
        .parameters
        .get("required_status_checks")
        .and_then(serde_json::Value::as_array)
        .context("required_status_checks lists no checks")?
        .iter()
        .filter_map(|check| check.get("context").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    let jobs = required_checks(workflows)?;
    let missing: Vec<&String> = jobs.difference(&required).collect();
    let unknown: Vec<&String> = required.difference(&jobs).collect();
    ensure!(
        missing.is_empty() && unknown.is_empty(),
        "{RULESET} is out of step with CI: jobs not required: {missing:?}; required checks no job \
         reports: {unknown:?}. Required checks must be the jobs of workflows that run on both \
         pull_request and merge_group."
    );
    ensure!(!ruleset.name.is_empty(), "the ruleset needs a name");
    Ok(())
}

/// The workflows in `.github/workflows`, as file names and texts.
fn workflows(root: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let dir = root.join(".github/workflows");
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("could not list {}", dir.display()))? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "yml" || extension == "yaml")
        {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            found.push((path.display().to_string(), text));
        }
    }
    found.sort();
    Ok(found)
}

/// `cargo xtask rulesets check`.
pub(crate) fn check(root: &Path) -> anyhow::Result<()> {
    let ruleset = fs::read_to_string(root.join(RULESET))
        .with_context(|| format!("could not read {RULESET}"))?;
    check_text(&ruleset, &workflows(root)?)
}

/// One existing ruleset on GitHub.
#[derive(Debug, Deserialize)]
struct Existing {
    id: u64,
    name: String,
}

/// A change `apply` makes, in order.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Create,
    Update(u64),
    Delete { id: u64, name: String },
}

/// The changes that put a ruleset named `name` on GitHub and remove the ones it replaces:
/// create or update first, so `main` is never unprotected, then delete.
fn plan(name: &str, existing: &[Existing]) -> Vec<Action> {
    let mut actions = vec![
        existing
            .iter()
            .find(|ruleset| ruleset.name == name)
            .map_or(Action::Create, |ruleset| Action::Update(ruleset.id)),
    ];
    actions.extend(
        existing
            .iter()
            .filter(|ruleset| REPLACED.contains(&ruleset.name.as_str()))
            .map(|ruleset| Action::Delete {
                id: ruleset.id,
                name: ruleset.name.clone(),
            }),
    );
    actions
}

/// The `owner/repo` of a GitHub remote URL.
fn repository(url: &str) -> anyhow::Result<String> {
    let path = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("git@github.com:"))
        .or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))
        .with_context(|| format!("origin is not on GitHub: {url}"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    ensure!(
        path.split('/').count() == 2,
        "origin is not an owner/repo URL: {url}"
    );
    Ok(path.to_owned())
}

/// `cargo xtask rulesets apply`: checks the ruleset, then applies it with the user's own `gh`.
pub(crate) fn apply(root: &Path, gh: &Path, out: &mut impl std::io::Write) -> anyhow::Result<()> {
    check(root)?;
    let repository = repository(&git::git(root, &["remote", "get-url", "origin"])?)?;
    let ruleset: Ruleset = serde_json::from_str(&fs::read_to_string(root.join(RULESET))?)?;
    let listing = run_gh(gh, root, &["api", &format!("repos/{repository}/rulesets")])?;
    let existing: Vec<Existing> =
        serde_json::from_str(&listing).context("could not read GitHub's rulesets")?;
    for action in plan(&ruleset.name, &existing) {
        match action {
            Action::Create => {
                run_gh(
                    gh,
                    root,
                    &[
                        "api",
                        "-X",
                        "POST",
                        &format!("repos/{repository}/rulesets"),
                        "--input",
                        RULESET,
                    ],
                )?;
                writeln!(out, "created the {} ruleset", ruleset.name)?;
            }
            Action::Update(id) => {
                run_gh(
                    gh,
                    root,
                    &[
                        "api",
                        "-X",
                        "PUT",
                        &format!("repos/{repository}/rulesets/{id}"),
                        "--input",
                        RULESET,
                    ],
                )?;
                writeln!(out, "updated the {} ruleset", ruleset.name)?;
            }
            Action::Delete { id, name } => {
                run_gh(
                    gh,
                    root,
                    &[
                        "api",
                        "-X",
                        "DELETE",
                        &format!("repos/{repository}/rulesets/{id}"),
                    ],
                )?;
                writeln!(out, "deleted the {name} ruleset it replaces")?;
            }
        }
    }
    Ok(())
}

fn run_gh(gh: &Path, root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(gh)
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("could not run {}", gh.display()))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("gh printed text that is not UTF-8")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{Action, Existing, apply, check, check_text, plan, repository, required_checks};
    use crate::testing::{git, repository as new_repository};

    const CI: &str =
        "on:\n  pull_request:\n  merge_group:\njobs:\n  check:\n    name: Checks\n  lint: {}\n";
    const ONLY_PR: &str = "on: [pull_request]\njobs:\n  label:\n    name: Labels\n";

    fn ruleset(checks: &[&str]) -> String {
        let required: Vec<serde_json::Value> = checks
            .iter()
            .map(|check| serde_json::json!({"context": check, "integration_id": 15368}))
            .collect();
        serde_json::json!({
            "name": "main",
            "rules": [
                {"type": "deletion"},
                {"type": "non_fast_forward"},
                {"type": "pull_request", "parameters": {
                    "required_approving_review_count": 1, "require_code_owner_review": true,
                    "dismiss_stale_reviews_on_push": true, "require_last_push_approval": true}},
                {"type": "required_status_checks", "parameters": {"required_status_checks": required}},
                {"type": "merge_queue", "parameters": {"merge_method": "SQUASH"}}
            ]
        })
        .to_string()
    }

    fn workflows() -> Vec<(String, String)> {
        vec![
            ("ci.yml".to_owned(), CI.to_owned()),
            ("label.yml".to_owned(), ONLY_PR.to_owned()),
        ]
    }

    #[test]
    fn requires_the_jobs_that_run_in_pull_requests_and_the_merge_queue() {
        assert_eq!(
            required_checks(&workflows())
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["Checks", "lint"],
            "a job without a name reports by its id; pull-request-only workflows are left out"
        );
    }

    #[test]
    fn accepts_a_ruleset_in_step_with_ci() {
        check_text(&ruleset(&["Checks", "lint"]), &workflows()).unwrap();
    }

    #[test]
    fn refuses_a_ruleset_out_of_step_with_ci() {
        let renamed = check_text(&ruleset(&["Checks", "Lint"]), &workflows())
            .unwrap_err()
            .to_string();
        assert!(
            renamed.contains(r#"jobs not required: ["lint"]"#),
            "{renamed}"
        );
        assert!(
            renamed.contains(r#"required checks no job reports: ["Lint"]"#),
            "{renamed}"
        );
        let weak = ruleset(&["Checks", "lint"]).replace(
            r#""require_code_owner_review":true"#,
            r#""require_code_owner_review":false"#,
        );
        assert!(
            check_text(&weak, &workflows())
                .unwrap_err()
                .to_string()
                .contains("require_code_owner_review"),
            "weakened review"
        );
        let merging = ruleset(&["Checks", "lint"]).replace("SQUASH", "MERGE");
        assert!(
            check_text(&merging, &workflows())
                .unwrap_err()
                .to_string()
                .contains("squash"),
            "merge commits"
        );
        let unprotected = ruleset(&["Checks", "lint"]).replace(r#"{"type":"deletion"},"#, "");
        assert!(
            check_text(&unprotected, &workflows())
                .unwrap_err()
                .to_string()
                .contains("no deletion rule"),
            "deletion allowed"
        );
    }

    #[test]
    fn the_checked_in_ruleset_matches_the_workflows() {
        check(&crate::repo_root().unwrap()).unwrap();
    }

    #[test]
    fn plans_the_replacement() {
        let existing = [
            Existing {
                id: 1,
                name: "Protect main".to_owned(),
            },
            Existing {
                id: 2,
                name: "main: checks must pass".to_owned(),
            },
            Existing {
                id: 3,
                name: "release".to_owned(),
            },
        ];
        assert_eq!(
            plan("main", &existing),
            [
                Action::Create,
                Action::Delete {
                    id: 1,
                    name: "Protect main".to_owned()
                },
                Action::Delete {
                    id: 2,
                    name: "main: checks must pass".to_owned()
                },
            ],
            "create first, then delete what it replaces, never other rulesets"
        );
        assert_eq!(
            plan(
                "main",
                &[Existing {
                    id: 9,
                    name: "main".to_owned()
                }]
            ),
            [Action::Update(9)],
            "an applied ruleset is updated"
        );
    }

    #[test]
    fn reads_the_repository_from_origin() {
        for url in [
            "https://github.com/getabc-dev/abc.git",
            "git@github.com:getabc-dev/abc.git",
            "ssh://git@github.com/getabc-dev/abc",
        ] {
            assert_eq!(repository(url).unwrap(), "getabc-dev/abc", "{url}");
        }
        assert!(
            repository("https://gitlab.com/x/y.git").is_err(),
            "not GitHub"
        );
        assert!(
            repository("https://github.com/x").is_err(),
            "not owner/repo"
        );
    }

    #[test]
    fn applies_with_the_users_gh() {
        let temp = tempfile::tempdir().unwrap();
        let repo = new_repository(&temp.path().join("repo"));
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/getabc-dev/abc.git",
            ],
        );
        fs::create_dir_all(repo.join(".github/workflows")).unwrap();
        fs::create_dir_all(repo.join(".github/rulesets")).unwrap();
        fs::write(repo.join(".github/workflows/ci.yml"), CI).unwrap();
        fs::write(
            repo.join(".github/rulesets/main.json"),
            ruleset(&["Checks", "lint"]),
        )
        .unwrap();
        let log = temp.path().join("gh.log");
        let gh = temp.path().join("gh");
        fs::write(&gh, format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncase \"$*\" in 'api repos/getabc-dev/abc/rulesets') echo '[{{\"id\": 1, \"name\": \"Protect main\"}}, {{\"id\": 2, \"name\": \"main: checks must pass\"}}]' ;; esac\n",
            log.display()
        ))
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let mut out = Vec::new();
        apply(&repo, &gh, &mut out).unwrap();
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "api repos/getabc-dev/abc/rulesets\n\
             api -X POST repos/getabc-dev/abc/rulesets --input .github/rulesets/main.json\n\
             api -X DELETE repos/getabc-dev/abc/rulesets/1\n\
             api -X DELETE repos/getabc-dev/abc/rulesets/2\n",
            "list, create, then delete the replaced rulesets"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("deleted the Protect main ruleset"),
            "reported"
        );
        fs::write(&gh, "#!/bin/sh\necho denied >&2\nexit 1\n").unwrap();
        let error = apply(&repo, &gh, &mut Vec::new()).unwrap_err().to_string();
        assert!(error.contains("denied"), "gh's error is passed on: {error}");
    }
}
