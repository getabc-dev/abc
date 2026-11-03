//! Claude Code in agent VMs runs on the maintainer's subscription only (D37).
//!
//! The VM gets one credential for Claude Code: the subscription token `claude setup-token` made
//! (`CLAUDE_CODE_OAUTH_TOKEN`). [`check`] refuses every other way Claude Code could reach a model
//! and bill an API account instead: API keys and auth tokens, key helpers, custom endpoints and
//! cloud providers, in the environment or in any settings file. The VM runs it before the agent
//! user starts anything and before each session, and the agent's `PreToolUse` hook before each tool
//! call. [`subscription`] reads `claude auth status --json` and accepts only the subscription
//! token.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail, ensure};

/// Environment variables that point Claude Code at an API account, a custom endpoint or a cloud
/// provider.
pub(crate) const FORBIDDEN_ENV: [&str; 13] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
    // Moves Claude Code's settings and credentials somewhere this check does not read.
    "CLAUDE_CONFIG_DIR",
];

/// Settings keys, anywhere in a settings file, that give Claude Code a credential other than the
/// subscription token.
const FORBIDDEN_KEYS: [&str; 4] = [
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "primaryApiKey",
];

/// The `authMethod` that `claude auth status` reports for a `claude setup-token` token.
pub(crate) const SUBSCRIPTION_TOKEN: &str = "oauth_token";

/// Where managed settings live on Linux: a file, and a directory of drop-ins.
const MANAGED: &str = "/etc/claude-code";

/// The settings files Claude Code reads for a session in `worktree` with home `home`, managed
/// settings under `managed` included. `claude --settings <file>` names one more, which no check
/// before a run can see; the check before each tool call is a tripwire for it.
pub(crate) fn settings_files(managed: &Path, home: &Path, worktree: &Path) -> Vec<PathBuf> {
    let mut files = vec![managed.join("managed-settings.json")];
    if let Ok(entries) = std::fs::read_dir(managed.join("managed-settings.d")) {
        let mut drop_ins: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        drop_ins.sort();
        files.extend(drop_ins);
    }
    files.extend([
        home.join(".claude/settings.json"),
        home.join(".claude/settings.local.json"),
        home.join(".claude.json"),
        worktree.join(".claude/settings.json"),
        worktree.join(".claude/settings.local.json"),
    ]);
    files
}

/// The managed settings directory of this machine.
pub(crate) fn managed() -> &'static Path {
    Path::new(MANAGED)
}

/// Fails, naming each finding, unless nothing in the environment `env` or in `files` (those that
/// exist) gives Claude Code a way to bill anything but the subscription.
pub(crate) fn check(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    files: &[PathBuf],
) -> anyhow::Result<()> {
    let mut findings: Vec<String> = FORBIDDEN_ENV
        .iter()
        .filter(|name| env(name).is_some())
        .map(|name| format!("{name} in the environment"))
        .collect();
    for file in files {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                findings.push(format!("{} cannot be read: {error}", file.display()));
                continue;
            }
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(settings) => findings.extend(
                forbidden_in(&settings)
                    .into_iter()
                    .map(|key| format!("{key} in {}", file.display())),
            ),
            Err(error) => findings.push(format!("{} is not JSON: {error}", file.display())),
        }
    }
    ensure!(
        findings.is_empty(),
        "Claude Code here may use the subscription token only, but found: {}",
        findings.join("; ")
    );
    Ok(())
}

/// The forbidden keys in `settings`: credential keys at any depth, and forbidden variables in an
/// `env` object at any depth.
fn forbidden_in(settings: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();
    walk(settings, "", &mut found);
    found
}

fn walk(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let here = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let in_env = path.rsplit('.').next() == Some("env");
                if FORBIDDEN_KEYS.contains(&key.as_str())
                    || (in_env && FORBIDDEN_ENV.contains(&key.as_str()))
                {
                    found.push(here.clone());
                }
                walk(child, &here, found);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                walk(child, &format!("{path}[{index}]"), found);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

/// What `claude auth status --json` reported, without anything secret.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) logged_in: bool,
    pub(crate) auth_method: String,
    pub(crate) api_provider: String,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "loggedIn {}, authMethod {}, apiProvider {}",
            self.logged_in, self.auth_method, self.api_provider
        )
    }
}

/// Reads `claude auth status --json` and accepts only a signed-in subscription token on
/// Anthropic's own API.
pub(crate) fn subscription(output: &str) -> anyhow::Result<Status> {
    let report: serde_json::Value =
        serde_json::from_str(output).context("`claude auth status --json` printed no JSON")?;
    let text = |key: &str| {
        report
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned()
    };
    let status = Status {
        logged_in: report
            .get("loggedIn")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        auth_method: text("authMethod"),
        api_provider: text("apiProvider"),
    };
    if !(status.logged_in
        && status.auth_method == SUBSCRIPTION_TOKEN
        && status.api_provider == "firstParty")
    {
        bail!("Claude Code is not on the subscription token ({status})");
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;

    use super::{check, settings_files, subscription};

    fn none(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn a_clean_session_passes() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"env": {"GIT_CONFIG_COUNT": "1"}, "hooks": {}, "permissions": {"deny": []}}"#,
        )
        .unwrap();
        check(none, &[settings, dir.path().join("absent.json")]).unwrap();
    }

    #[test]
    fn refuses_api_credentials_in_the_environment() {
        let error = format!(
            "{:#}",
            check(
                |name| (name == "ANTHROPIC_API_KEY" || name == "CLAUDE_CODE_USE_BEDROCK")
                    .then(|| OsString::from("x")),
                &[]
            )
            .unwrap_err()
        );
        assert!(
            error.ends_with(
                "found: ANTHROPIC_API_KEY in the environment; CLAUDE_CODE_USE_BEDROCK in the \
                 environment"
            ),
            "{error}"
        );
    }

    #[test]
    fn refuses_credentials_and_routes_in_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.local.json");
        fs::write(
            &settings,
            r#"{"apiKeyHelper": "echo k", "env": {"ANTHROPIC_BASE_URL": "https://x", "PATH": "/bin"},
               "nested": {"env": {"ANTHROPIC_AUTH_TOKEN": "t"}, "list": [{"primaryApiKey": "k"}]},
               "other": {"ANTHROPIC_API_KEY": "only a forbidden name inside env counts"}}"#,
        )
        .unwrap();
        let error = format!(
            "{:#}",
            check(none, std::slice::from_ref(&settings)).unwrap_err()
        );
        let shown = settings.display();
        for finding in [
            format!("apiKeyHelper in {shown}"),
            format!("env.ANTHROPIC_BASE_URL in {shown}"),
            format!("nested.env.ANTHROPIC_AUTH_TOKEN in {shown}"),
            format!("nested.list[0].primaryApiKey in {shown}"),
        ] {
            assert!(error.contains(&finding), "{finding}: {error}");
        }
        assert!(!error.contains("other."), "{error}");
        assert!(!error.contains("PATH"), "{error}");
    }

    #[test]
    fn fails_closed_on_files_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{").unwrap();
        let error = format!(
            "{:#}",
            check(none, &[broken, dir.path().to_path_buf()]).unwrap_err()
        );
        assert!(error.contains("is not JSON"), "{error}");
        assert!(error.contains("cannot be read"), "a directory: {error}");
    }

    #[test]
    fn reads_the_files_claude_code_reads() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path();
        fs::create_dir_all(managed.join("managed-settings.d")).unwrap();
        for name in ["20-b.json", "10-a.json", "notes.txt"] {
            fs::write(managed.join("managed-settings.d").join(name), "{}").unwrap();
        }
        let files: Vec<_> = settings_files(managed, "/home/agent".as_ref(), "/w".as_ref())
            .into_iter()
            .map(|file| {
                file.strip_prefix(managed)
                    .map_or_else(|_| file.clone(), Path::to_path_buf)
            })
            .collect();
        let shown: Vec<_> = files.iter().map(|f| f.display().to_string()).collect();
        assert_eq!(
            shown,
            [
                "managed-settings.json",
                "managed-settings.d/10-a.json",
                "managed-settings.d/20-b.json",
                "/home/agent/.claude/settings.json",
                "/home/agent/.claude/settings.local.json",
                "/home/agent/.claude.json",
                "/w/.claude/settings.json",
                "/w/.claude/settings.local.json",
            ]
        );
    }

    #[test]
    fn accepts_only_the_subscription_token() {
        let status = subscription(
            r#"{"loggedIn": true, "authMethod": "oauth_token", "apiProvider": "firstParty",
                "email": "someone@example.com"}"#,
        )
        .unwrap();
        assert_eq!(
            status.to_string(),
            "loggedIn true, authMethod oauth_token, apiProvider firstParty",
            "nothing else is repeated"
        );
        for refused in [
            r#"{"loggedIn": false, "authMethod": "none", "apiProvider": "firstParty"}"#,
            r#"{"loggedIn": true, "authMethod": "api_key", "apiProvider": "firstParty"}"#,
            r#"{"loggedIn": true, "authMethod": "oauth_token", "apiProvider": "bedrock"}"#,
            r#"{"authMethod": "oauth_token", "apiProvider": "firstParty"}"#,
            r"{}",
        ] {
            let error = format!("{:#}", subscription(refused).unwrap_err());
            assert!(
                error.contains("not on the subscription token"),
                "{refused}: {error}"
            );
        }
        let garbage = format!("{:#}", subscription("Not logged in").unwrap_err());
        assert!(garbage.contains("printed no JSON"), "{garbage}");
    }
}
