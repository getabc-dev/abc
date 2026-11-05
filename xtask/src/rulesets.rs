//! Repository rules as code, in `.github/rulesets/`: `main.json` for `main`, `branches.json`,
//! which leaves the agent App only `agent/*` branches to write, and `tags.json`, which leaves tags
//! to the maintainers.
//!
//! `cargo xtask rulesets check` keeps the ruleset in step with CI: its required status checks
//! must be exactly the jobs of the workflows that run on both pull requests and the merge queue,
//! since only those can report in both places, so renaming or adding a job fails the build. A
//! matrix job reports under a name per entry, so it is never required itself; a job that needs it
//! reports for all of them. Every required job, with the jobs it waits for, must finish within the
//! merge queue's check timeout. The only bypass is the break-glass one: repository admins, through
//! a pull request, which GitHub logs.
//! The other two must keep their shape: every ref but the excluded ones restricted, and no bypass
//! but the maintainers' (and Renovate's, for its own branches); the agent App is never one.
//! `cargo xtask rulesets apply`, run by the user with their own gh login, puts the files on
//! GitHub in place of the rulesets they replace.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use yaml_rust2::{Yaml, YamlLoader};

use crate::git;

/// The ruleset file, relative to the repository root.
pub(crate) const RULESET: &str = ".github/rulesets/main.json";

/// The branch ruleset that leaves the agent App only `agent/*` branches, and the tag ruleset.
pub(crate) const BRANCHES: &str = ".github/rulesets/branches.json";
pub(crate) const TAGS: &str = ".github/rulesets/tags.json";

/// The rulesets `main.json` replaces on GitHub.
const REPLACED: [&str; 2] = ["Protect main", "main: checks must pass"];

/// The getabc-dev-agent App's id on GitHub, never a bypass actor.
const AGENT_APP: u64 = 5_556_986;

/// Renovate's App id on GitHub: it pushes its own `renovate/*` branches.
const RENOVATE_APP: u64 = 2740;

/// The parts of the ruleset the check reads.
#[derive(Deserialize)]
struct Ruleset {
    name: String,
    #[serde(default)]
    bypass_actors: serde_json::Value,
    rules: Vec<Rule>,
}

/// GitHub's own default for a job without `timeout-minutes`.
const DEFAULT_JOB_MINUTES: u64 = 360;

/// The break-glass bypass: repository admins (role 5), through a pull request only.
fn break_glass() -> serde_json::Value {
    serde_json::json!([
        {"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "pull_request"}
    ])
}

#[derive(Deserialize)]
struct Rule {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    parameters: serde_json::Value,
}

/// The status checks a ruleset must require, with the minutes each may take including the jobs
/// it waits for: the jobs, other than matrix jobs, of every workflow in `workflows` that runs on
/// both pull requests (`pull_request` or `pull_request_target`) and `merge_group`.
fn required_checks(workflows: &[(String, String)]) -> anyhow::Result<BTreeMap<String, u64>> {
    let mut checks = BTreeMap::new();
    for (file, text) in workflows {
        let documents =
            YamlLoader::load_from_str(text).with_context(|| format!("{file} is not YAML"))?;
        let Some(workflow) = documents.first() else {
            continue;
        };
        let triggers = triggers(&workflow["on"]);
        // A job holding secrets runs on pull_request_target, from the base's workflow.
        let on_pull_requests =
            triggers.contains("pull_request") || triggers.contains("pull_request_target");
        if !(on_pull_requests && triggers.contains("merge_group")) {
            continue;
        }
        let jobs = workflow["jobs"]
            .as_hash()
            .with_context(|| format!("{file} has no jobs"))?;
        for (id, job) in jobs {
            if !job["strategy"]["matrix"].is_badvalue() {
                continue;
            }
            let name = job["name"]
                .as_str()
                .or_else(|| id.as_str())
                .with_context(|| format!("{file} has a job without a name"))?;
            let id = id.as_str().context("a job id is not a string")?;
            checks.insert(name.to_owned(), minutes(&workflow["jobs"], id, 0)?);
        }
    }
    Ok(checks)
}

/// The longest the job `id` in `jobs` can take: its own timeout plus that of the slowest job it
/// needs, recursively; `depth` stops a cycle.
fn minutes(jobs: &Yaml, id: &str, depth: usize) -> anyhow::Result<u64> {
    ensure!(depth <= 32, "the needs of job {id} form a cycle");
    let job = &jobs[id];
    ensure!(!job.is_badvalue(), "no job {id}");
    let own = job["timeout-minutes"]
        .as_i64()
        .map_or(Ok(DEFAULT_JOB_MINUTES), u64::try_from)
        .with_context(|| format!("job {id} has a negative timeout"))?;
    let needs: Vec<&str> = match &job["needs"] {
        Yaml::String(need) => vec![need.as_str()],
        Yaml::Array(needs) => needs.iter().filter_map(Yaml::as_str).collect(),
        Yaml::Real(_)
        | Yaml::Integer(_)
        | Yaml::Boolean(_)
        | Yaml::Hash(_)
        | Yaml::Alias(_)
        | Yaml::Null
        | Yaml::BadValue => Vec::new(),
    };
    let mut slowest: u64 = 0;
    for need in needs {
        slowest = slowest.max(minutes(jobs, need, depth.saturating_add(1))?);
    }
    Ok(own.saturating_add(slowest))
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
    let checks = required_checks(workflows)?;
    let jobs: BTreeSet<String> = checks.keys().cloned().collect();
    let missing: Vec<&String> = jobs.difference(&required).collect();
    let unknown: Vec<&String> = required.difference(&jobs).collect();
    ensure!(
        missing.is_empty() && unknown.is_empty(),
        "{RULESET} is out of step with CI: jobs not required: {missing:?}; required checks no job \
         reports: {unknown:?}. Required checks must be the jobs of workflows that run on both \
         pull_request and merge_group."
    );
    let queue_minutes = rule("merge_queue")
        .context("checked above")?
        .parameters
        .get("check_response_timeout_minutes")
        .and_then(serde_json::Value::as_u64)
        .context("the merge queue needs check_response_timeout_minutes")?;
    let slow: Vec<String> = checks
        .iter()
        .filter(|&(_, &minutes)| minutes > queue_minutes)
        .map(|(name, minutes)| format!("{name} ({minutes} min)"))
        .collect();
    ensure!(
        slow.is_empty(),
        "required checks can outlast the merge queue's {queue_minutes}-minute timeout, so a \
         queued run could never finish: {}",
        slow.join(", ")
    );
    ensure!(
        ruleset.bypass_actors == break_glass(),
        "the ruleset's only bypass must be the break-glass one: {}",
        break_glass()
    );
    ensure!(!ruleset.name.is_empty(), "the ruleset needs a name");
    Ok(())
}

/// The gates whose runs include the tests, as `cargo xtask` subcommands.
const TEST_GATES: [&str; 3] = ["check", "coverage", "mutants"];

/// Fails when a job of `workflows` runs a gate that runs the tests without first installing the
/// tools the tests need with `cargo xtask setup --for-tests`.
fn installs_test_tools(workflows: &[(String, String)]) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    for (file, text) in workflows {
        let documents =
            YamlLoader::load_from_str(text).with_context(|| format!("{file} is not YAML"))?;
        let Some(jobs) = documents
            .first()
            .and_then(|workflow| workflow["jobs"].as_hash())
        else {
            continue;
        };
        for (id, job) in jobs {
            let mut ready = false;
            for run in job["steps"]
                .as_vec()
                .into_iter()
                .flatten()
                .filter_map(|step| step["run"].as_str())
            {
                let commands: Vec<Vec<&str>> = run
                    .lines()
                    .map(|line| line.split_whitespace().collect())
                    .collect();
                for words in commands {
                    match words.as_slice() {
                        ["cargo", "xtask", "setup", rest @ ..] if rest.contains(&"--for-tests") => {
                            ready = true;
                        }
                        ["cargo", "xtask", gate, ..] if !ready && TEST_GATES.contains(gate) => {
                            missing.push(format!(
                                "{file}: job {} runs `cargo xtask {gate}`",
                                id.as_str().unwrap_or("?")
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    ensure!(
        missing.is_empty(),
        "these jobs run the tests without the tools they need; run `cargo xtask setup \
         --for-tests` in an earlier step: {}",
        missing.join("; ")
    );
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
    let read = |file: &str| {
        fs::read_to_string(root.join(file)).with_context(|| format!("could not read {file}"))
    };
    let workflows = workflows(root)?;
    check_text(&read(RULESET)?, &workflows)?;
    installs_test_tools(&workflows)?;
    check_refs(&read(BRANCHES)?, Refs::Branches).with_context(|| BRANCHES.to_owned())?;
    check_refs(&read(TAGS)?, Refs::Tags).with_context(|| TAGS.to_owned())
}

/// Which refs a restricting ruleset covers.
#[derive(Clone, Copy)]
enum Refs {
    Branches,
    Tags,
}

/// Checks a ruleset that restricts writing refs: all of them, but for the exclusions its kind
/// needs, with no bypass but the maintainers' (and Renovate's, for branches).
fn check_refs(text: &str, refs: Refs) -> anyhow::Result<()> {
    let ruleset: serde_json::Value =
        serde_json::from_str(text).context("the ruleset is not JSON")?;
    let admins =
        serde_json::json!({"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"});
    let renovate = serde_json::json!({"actor_type": "Integration", "actor_id": RENOVATE_APP, "bypass_mode": "always"});
    let (target, excluded, bypass, rules) = match refs {
        Refs::Branches => (
            "branch",
            // GitHub matches a ruleset pattern one path segment per `*`; even `**` never crosses a
            // `/`. The merge queue's branches are `gh-readonly-queue/main/pr-<n>-<sha>`, three
            // segments, and agents' are `agent/<topic>`, two.
            serde_json::json!([
                "~DEFAULT_BRANCH",
                "refs/heads/agent/*",
                "refs/heads/gh-readonly-queue/main/*"
            ]),
            serde_json::json!([admins, renovate]),
            serde_json::json!([{"type": "creation"}, {"type": "update"}, {"type": "deletion"}, {"type": "non_fast_forward"}]),
        ),
        Refs::Tags => (
            "tag",
            serde_json::json!([]),
            serde_json::json!([admins]),
            serde_json::json!([{"type": "creation"}, {"type": "update"}, {"type": "deletion"}]),
        ),
    };
    let field = |path: &[&str]| {
        path.iter()
            .try_fold(&ruleset, |value, key| value.get(key))
            .cloned()
            .unwrap_or_default()
    };
    ensure!(
        field(&["name"])
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "the ruleset needs a name"
    );
    ensure!(
        field(&["target"]) == target,
        "the ruleset must target {target}es"
    );
    ensure!(
        field(&["enforcement"]) == "active",
        "the ruleset must be active"
    );
    ensure!(
        field(&["conditions", "ref_name", "include"]) == serde_json::json!(["~ALL"])
            && field(&["conditions", "ref_name", "exclude"]) == excluded,
        "the ruleset must cover every ref but {excluded}"
    );
    ensure!(
        field(&["bypass_actors"]) == bypass,
        "the ruleset's only bypass must be {bypass}"
    );
    ensure!(
        !text.contains(&AGENT_APP.to_string()),
        "the agent App must never bypass a ruleset"
    );
    ensure!(
        field(&["rules"]) == rules,
        "the ruleset's rules must be {rules}"
    );
    Ok(())
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

/// The changes that put a ruleset named `name` on GitHub and remove the `replaced` ones: create
/// or update first, so the refs are never unprotected, then delete.
fn plan(name: &str, replaced: &[&str], existing: &[Existing]) -> Vec<Action> {
    let mut actions = vec![
        existing
            .iter()
            .find(|ruleset| ruleset.name == name)
            .map_or(Action::Create, |ruleset| Action::Update(ruleset.id)),
    ];
    actions.extend(
        existing
            .iter()
            .filter(|ruleset| replaced.contains(&ruleset.name.as_str()))
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

/// `cargo xtask rulesets apply`: checks the rulesets, then applies them with the user's own `gh`.
pub(crate) fn apply(root: &Path, gh: &Path, out: &mut impl std::io::Write) -> anyhow::Result<()> {
    check(root)?;
    let repository = repository(&git::git(root, &["remote", "get-url", "origin"])?)?;
    for (file, replaced) in [(RULESET, &REPLACED[..]), (BRANCHES, &[]), (TAGS, &[])] {
        let ruleset: Ruleset = serde_json::from_str(&fs::read_to_string(root.join(file))?)?;
        let listing = run_gh(gh, root, &["api", &format!("repos/{repository}/rulesets")])?;
        let existing: Vec<Existing> =
            serde_json::from_str(&listing).context("could not read GitHub's rulesets")?;
        for action in plan(&ruleset.name, replaced, &existing) {
            let (method, path, done) = match &action {
                Action::Create => (
                    "POST",
                    format!("repos/{repository}/rulesets"),
                    format!("created the {} ruleset", ruleset.name),
                ),
                Action::Update(id) => (
                    "PUT",
                    format!("repos/{repository}/rulesets/{id}"),
                    format!("updated the {} ruleset", ruleset.name),
                ),
                Action::Delete { id, name } => (
                    "DELETE",
                    format!("repos/{repository}/rulesets/{id}"),
                    format!("deleted the {name} ruleset it replaces"),
                ),
            };
            let mut args = vec!["api", "-X", method, path.as_str()];
            if !matches!(action, Action::Delete { .. }) {
                args.extend(["--input", file]);
            }
            run_gh(gh, root, &args)?;
            writeln!(out, "{done}")?;
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut reason = stderr.trim().to_owned();
        if let Some(explained) = github_error(&String::from_utf8_lossy(&output.stdout)) {
            reason.push_str("; GitHub says: ");
            reason.push_str(&explained);
        }
        bail!("gh {} failed: {reason}", args.join(" "));
    }
    String::from_utf8(output.stdout).context("gh printed text that is not UTF-8")
}

/// GitHub's own explanation of a failed API call, from the error body `gh api` prints on stdout:
/// its `message`, then each entry of `errors`, which hold the actual reason behind a bare
/// "Validation Failed". Entries are strings or objects with a `message` (else their `code`,
/// `field` and `resource`). `None` when stdout holds no such body.
fn github_error(stdout: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let message = body.get("message").and_then(serde_json::Value::as_str);
    let errors: Vec<String> = body
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .map(|errors| errors.iter().filter_map(error_entry).collect())
        .unwrap_or_default();
    match (message, errors.is_empty()) {
        (None, true) => None,
        (Some(message), true) => Some(message.to_owned()),
        (None, false) => Some(errors.join("; ")),
        (Some(message), false) => Some(format!("{message}: {}", errors.join("; "))),
    }
}

/// One entry of a GitHub error body's `errors`, as text.
fn error_entry(entry: &serde_json::Value) -> Option<String> {
    if let Some(text) = entry.as_str() {
        return Some(text.to_owned());
    }
    let field = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
    if let Some(message) = field("message") {
        return Some(message.to_owned());
    }
    let parts: Vec<String> = ["resource", "field", "code"]
        .into_iter()
        .filter_map(|name| field(name).map(|value| format!("{name} {value}")))
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{
        Action, BRANCHES, Existing, REPLACED, Refs, TAGS, apply, check, check_refs, check_text,
        github_error, installs_test_tools, plan, repository, required_checks,
    };
    use crate::testing::{git, repository as new_repository};

    const CI: &str = "on:\n  pull_request:\n  merge_group:\njobs:\n  check:\n    name: Checks\n    timeout-minutes: 30\n  shard:\n    timeout-minutes: 60\n    strategy:\n      matrix:\n        shard: [0, 1]\n  lint:\n    needs: [shard]\n    timeout-minutes: 5\n";
    const ONLY_PR: &str = "on: [pull_request]\njobs:\n  label:\n    name: Labels\n";

    fn ruleset(checks: &[&str]) -> String {
        let required: Vec<serde_json::Value> = checks
            .iter()
            .map(|check| serde_json::json!({"context": check, "integration_id": 15368}))
            .collect();
        serde_json::json!({
            "name": "main",
            "bypass_actors": [
                {"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "pull_request"}
            ],
            "rules": [
                {"type": "deletion"},
                {"type": "non_fast_forward"},
                {"type": "pull_request", "parameters": {
                    "required_approving_review_count": 1, "require_code_owner_review": true,
                    "dismiss_stale_reviews_on_push": true, "require_last_push_approval": true}},
                {"type": "required_status_checks", "parameters": {"required_status_checks": required}},
                {"type": "merge_queue", "parameters": {
                    "merge_method": "SQUASH", "check_response_timeout_minutes": 150}}
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
            [("Checks".to_owned(), 30), ("lint".to_owned(), 65)],
            "a job without a name reports by its id, with the jobs it needs; matrix jobs and \
             pull-request-only workflows are left out"
        );
        let target = vec![(
            "review.yml".to_owned(),
            "on:\n  pull_request_target:\n  merge_group:\njobs:\n  gemini:\n    name: Gemini review\n    timeout-minutes: 20\n".to_owned(),
        )];
        assert_eq!(
            required_checks(&target)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [("Gemini review".to_owned(), 20)],
            "a job on pull_request_target runs on pull requests too"
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
        let exact = ruleset(&["Checks", "lint"]).replace(
            r#""check_response_timeout_minutes":150"#,
            r#""check_response_timeout_minutes":65"#,
        );
        check_text(&exact, &workflows()).unwrap();
        let slow = ruleset(&["Checks", "lint"]).replace(
            r#""check_response_timeout_minutes":150"#,
            r#""check_response_timeout_minutes":60"#,
        );
        assert!(
            check_text(&slow, &workflows())
                .unwrap_err()
                .to_string()
                .contains("lint (65 min)"),
            "a check that outlasts the queue"
        );
        let open = ruleset(&["Checks", "lint"]).replace(
            r#""bypass_mode":"pull_request""#,
            r#""bypass_mode":"always""#,
        );
        assert!(
            check_text(&open, &workflows())
                .unwrap_err()
                .to_string()
                .contains("break-glass"),
            "a bypass that skips pull requests"
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
    fn checks_the_files_in_a_repository() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::create_dir_all(root.join(".github/rulesets")).unwrap();
        fs::write(root.join(".github/workflows/ci.yaml"), CI).unwrap();
        fs::write(
            root.join(".github/workflows/notes.md"),
            "# not a workflow: [",
        )
        .unwrap();
        fs::write(
            root.join(".github/rulesets/main.json"),
            ruleset(&["Checks", "lint"]),
        )
        .unwrap();
        let checked_in = crate::repo_root().unwrap();
        for file in [BRANCHES, TAGS] {
            fs::copy(checked_in.join(file), root.join(file)).unwrap();
        }
        check(root).unwrap();
        fs::write(
            root.join(".github/rulesets/main.json"),
            ruleset(&["Checks"]),
        )
        .unwrap();
        let error = format!("{:#}", check(root).unwrap_err());
        assert!(
            error.contains("lint"),
            "a job missing from the ruleset: {error}"
        );
    }

    #[test]
    fn a_job_that_runs_the_tests_installs_their_tools_first() {
        let job = |steps: &str| {
            vec![(
                "ci.yml".to_owned(),
                format!("on: [pull_request]\njobs:\n  coverage:\n    steps:\n{steps}"),
            )]
        };
        // #25's Coverage job: it installed cargo-nextest alone, and the tests needed shellcheck.
        let error = format!(
            "{:#}",
            installs_test_tools(&job(
                "      - run: cargo xtask setup cargo-nextest\n      - run: cargo xtask coverage --base \"$BASE\"\n"
            ))
            .unwrap_err()
        );
        assert!(
            error.contains("ci.yml: job coverage runs `cargo xtask coverage`"),
            "{error}"
        );
        assert!(
            installs_test_tools(&job(
                "      - run: cargo xtask check\n      - run: cargo xtask setup --for-tests\n"
            ))
            .is_err(),
            "installed too late"
        );
        installs_test_tools(&job(
            "      - uses: ./a\n      - run: |\n          cargo xtask setup --for-tests cargo-sqlx\n          cargo xtask mutants --shard 0/8\n",
        ))
        .unwrap();
        installs_test_tools(&job("      - run: cargo xtask review --base x\n")).unwrap();
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
            plan("main", &REPLACED, &existing),
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
                &REPLACED,
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
        let checked_in = crate::repo_root().unwrap();
        for file in [BRANCHES, TAGS] {
            fs::copy(checked_in.join(file), repo.join(file)).unwrap();
        }
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
             api -X DELETE repos/getabc-dev/abc/rulesets/2\n\
             api repos/getabc-dev/abc/rulesets\n\
             api -X POST repos/getabc-dev/abc/rulesets --input .github/rulesets/branches.json\n\
             api repos/getabc-dev/abc/rulesets\n\
             api -X POST repos/getabc-dev/abc/rulesets --input .github/rulesets/tags.json\n",
            "list, create, then delete the replaced rulesets; then the branch and tag rulesets"
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
        // What `gh api` does when GitHub refuses a ruleset: the error body on stdout, a summary
        // on stderr.
        fs::write(
            &gh,
            "#!/bin/sh
case \"$*\" in 'api repos/getabc-dev/abc/rulesets') echo '[]' ;; *) \
             echo '{\"message\":\"Validation Failed\",\"errors\":[\"Actor Renovate integration \
             must be part of the ruleset source or owner organization\"],\"status\":\"422\"}'; \
             echo 'gh: Validation Failed (HTTP 422)' >&2; exit 1 ;; esac\n",
        )
        .unwrap();
        let refused = apply(&repo, &gh, &mut Vec::new()).unwrap_err().to_string();
        assert_eq!(
            refused,
            "gh api -X POST repos/getabc-dev/abc/rulesets --input .github/rulesets/main.json \
             failed: gh: Validation Failed (HTTP 422); GitHub says: Validation Failed: Actor \
             Renovate integration must be part of the ruleset source or owner organization",
            "GitHub's reason, not only gh's summary"
        );
    }

    #[test]
    fn reads_githubs_error_bodies() {
        assert_eq!(
            github_error(r#"{"message":"Validation Failed","errors":["a","b"]}"#).as_deref(),
            Some("Validation Failed: a; b"),
            "the message, then every error"
        );
        assert_eq!(
            github_error(
                r#"{"message":"Validation Failed","errors":[{"message":"Name is taken"},{"resource":"Ruleset","field":"name","code":"invalid"},{"other":1}]}"#
            )
            .as_deref(),
            Some("Validation Failed: Name is taken; resource Ruleset, field name, code invalid"),
            "an object's message, else its resource, field and code; an entry with none is left out"
        );
        assert_eq!(
            github_error(
                r#"{"message":"Not Found","documentation_url":"https://docs.github.com"}"#
            )
            .as_deref(),
            Some("Not Found"),
            "a message alone"
        );
        assert_eq!(
            github_error(r#"{"errors":["only errors"]}"#).as_deref(),
            Some("only errors"),
            "errors alone"
        );
        for silent in [
            "",
            "not json",
            "[]",
            r#"{"documentation_url":"x"}"#,
            r#"{"errors":[]}"#,
        ] {
            assert_eq!(github_error(silent), None, "{silent:?} explains nothing");
        }
    }

    #[test]
    fn the_branch_and_tag_rulesets_keep_their_shape() {
        let root = crate::repo_root().unwrap();
        let branches = fs::read_to_string(root.join(BRANCHES)).unwrap();
        let tags = fs::read_to_string(root.join(TAGS)).unwrap();
        check_refs(&branches, Refs::Branches).unwrap();
        check_refs(&tags, Refs::Tags).unwrap();
        let refused =
            |text: String, refs: Refs| format!("{:#}", check_refs(&text, refs).unwrap_err());
        for (changed, problem) in [
            (
                branches.replace("\"branch\"", "\"tag\""),
                "must target branches",
            ),
            (
                branches.replace("\"active\"", "\"evaluate\""),
                "must be active",
            ),
            (
                branches.replace("\"refs/heads/agent/*\", ", ""),
                "must cover every ref but",
            ),
            (
                branches.replace("\"actor_id\": 2740", "\"actor_id\": 5556986"),
                "only bypass must be",
            ),
            (
                branches.replace("{ \"type\": \"update\" },", ""),
                "rules must be",
            ),
            (
                branches.replace(
                    "\"name\": \"branches: agents write agent/* only\"",
                    "\"name\": \"\"",
                ),
                "needs a name",
            ),
        ] {
            let error = refused(changed, Refs::Branches);
            assert!(error.contains(problem), "{problem}: {error}");
        }
        let app = tags.replace(
            "\"bypass_mode\": \"always\" }",
            "\"bypass_mode\": \"always\" }, { \"actor_type\": \"Integration\", \"actor_id\": 5556986, \"bypass_mode\": \"always\" }",
        );
        assert!(refused(app, Refs::Tags).contains("only bypass must be"));
        assert!(refused("{".to_owned(), Refs::Tags).contains("not JSON"));
        check(&root).unwrap();
    }

    /// Whether GitHub's ruleset `pattern` matches `name`: one path segment per `*`, never across a
    /// `/`, which is how the live rules treat `**` too.
    fn github_matches(pattern: &str, name: &str) -> bool {
        let pattern: Vec<&str> = pattern.split('/').collect();
        let name: Vec<&str> = name.split('/').collect();
        pattern.len() == name.len()
            && pattern
                .iter()
                .zip(&name)
                .all(|(want, have)| *want == "*" || want == have)
    }

    #[test]
    fn the_branch_exclusions_free_the_merge_queue_and_agents_and_nothing_else() {
        let ruleset: serde_json::Value =
            serde_json::from_str(include_str!("../../.github/rulesets/branches.json")).unwrap();
        let excluded: Vec<String> = ruleset["conditions"]["ref_name"]["exclude"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pattern| pattern.as_str().unwrap().to_owned())
            .collect();
        let free = |name: &str| excluded.iter().any(|pattern| github_matches(pattern, name));
        assert!(
            free(
                "refs/heads/gh-readonly-queue/main/pr-20-be44699ab1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6"
            ),
            "the merge queue's branches, which #20's queue run could not create"
        );
        assert!(free("refs/heads/agent/s0-worker"), "an agent's branch");
        assert!(
            !free("refs/heads/feature"),
            "any other branch stays restricted"
        );
        assert!(
            !free("refs/heads/agent/a/b"),
            "agents' branches have one segment"
        );
    }
}
