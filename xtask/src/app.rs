//! Acting as the abc agent GitHub App (D3).
//!
//! Agents push, comment and open pull requests as the App, with installation tokens that expire
//! in an hour, never with the user's own credentials. Signing a token request takes the App's
//! client ID, its installation ID and its private key, kept outside the repository in
//! `~/.config/abc/agent-app/` (`app.json` and `private-key.pem`, which only its owner may read).
//! Agents never read the key: `.claude/settings.json` denies that directory to their sandboxed
//! commands, and only the token broker ([`crate::broker`]), which the user runs outside the
//! sandbox, loads it. Agents ask the broker for tokens instead.
//!
//! A token is minted for every use, scoped to the one repository and the fewest permissions that
//! use needs, and goes only where it is needed: to git through the credential-helper protocol, or
//! to `gh` in `GH_TOKEN`, revoked once gh exits. It is never printed or logged.
//!
//! Only agent sessions use the App: the committed `.claude/settings.json` sets git's credential
//! helper for github.com through `GIT_CONFIG_*` variables in their environment. The user's own
//! terminal keeps the user's credentials.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;

/// GitHub's REST API.
pub(crate) const GITHUB_API: &str = "https://api.github.com";

/// The App's settings in `app.json`.
#[derive(Deserialize)]
struct Settings {
    client_id: String,
    installation_id: u64,
    /// The one repository its tokens may reach, by name.
    #[serde(default = "default_repository")]
    repository: String,
}

fn default_repository() -> String {
    "abc".to_owned()
}

/// What a token is for, which decides the permissions it gets (I5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// git fetching and pushing, workflow files included.
    Git,
    /// gh: pull requests and the branches behind them, and reading CI results.
    Gh,
}

impl Scope {
    /// The scope's name in the broker's protocol.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Gh => "gh",
        }
    }

    /// The scope named `name`, if any.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        [Self::Git, Self::Gh]
            .into_iter()
            .find(|scope| scope.name() == name)
    }

    fn permissions(self) -> serde_json::Value {
        match self {
            Self::Git => serde_json::json!({"contents": "write", "workflows": "write"}),
            Self::Gh => serde_json::json!({
                "contents": "write",
                "pull_requests": "write",
                "actions": "read",
                "checks": "read",
                "statuses": "read",
            }),
        }
    }
}

/// The App, ready to sign token requests.
pub(crate) struct App {
    client_id: String,
    installation_id: u64,
    repository: String,
    key: RsaKeyPair,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("client_id", &self.client_id)
            .field("installation_id", &self.installation_id)
            .finish_non_exhaustive()
    }
}

/// Where the App's files live: `~/.config/abc/agent-app`, whatever `XDG_CONFIG_HOME` says, since
/// that is the path `.claude/settings.json` denies to agents.
pub(crate) fn config_dir() -> anyhow::Result<PathBuf> {
    config_dir_from(std::env::var_os("HOME"))
}

/// [`config_dir`] for the home directory `home`.
fn config_dir_from(home: Option<OsString>) -> anyhow::Result<PathBuf> {
    let home = home
        .filter(|home| !home.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(home).join(CONFIG_DIR))
}

/// The App's directory, relative to the home directory.
pub(crate) const CONFIG_DIR: &str = ".config/abc/agent-app";

impl App {
    /// Reads `app.json` and `private-key.pem` from `dir`.
    pub(crate) fn load(dir: &Path) -> anyhow::Result<Self> {
        let settings_path = dir.join("app.json");
        let settings: Settings = serde_json::from_str(
            &fs::read_to_string(&settings_path)
                .with_context(|| format!("could not read {}", settings_path.display()))?,
        )
        .with_context(|| {
            format!(
                "{} needs client_id and installation_id",
                settings_path.display()
            )
        })?;
        let key_path = dir.join("private-key.pem");
        let mode = fs::metadata(&key_path)
            .with_context(|| format!("could not read {}", key_path.display()))?
            .permissions()
            .mode();
        // The low six bits are the group's and others' read, write and execute.
        ensure!(
            mode.trailing_zeros() >= 6,
            "{} is readable by others (mode {:o}); run `chmod 600` on it",
            key_path.display(),
            mode & 0o777
        );
        let pem = fs::read_to_string(&key_path)
            .with_context(|| format!("could not read {}", key_path.display()))?;
        Ok(Self {
            client_id: settings.client_id,
            installation_id: settings.installation_id,
            repository: settings.repository,
            key: parse_key(&pem)
                .with_context(|| format!("{} is not an RSA key", key_path.display()))?,
        })
    }

    /// A JSON Web Token that identifies the App to GitHub for nine minutes from `now` (Unix
    /// seconds). It is backdated a minute against clock drift; GitHub allows at most ten.
    fn jwt(&self, now: u64) -> anyhow::Result<String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iat": now.saturating_sub(60),
            "exp": now.saturating_add(540),
            "iss": self.client_id,
        });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let message = format!("{header}.{payload}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                message.as_bytes(),
                &mut signature,
            )
            .map_err(|error| anyhow!("could not sign the App's token request: {error}"))?;
        Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    /// A fresh installation token from the GitHub API at `api`, reached through `agent`, valid
    /// for an hour, for the App's one repository with the permissions `scope` needs.
    pub(crate) fn token(
        &self,
        agent: &ureq::Agent,
        api: &str,
        scope: Scope,
    ) -> anyhow::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("the clock is before 1970")?
            .as_secs();
        let url = format!(
            "{api}/app/installations/{}/access_tokens",
            self.installation_id
        );
        let mut response = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.jwt(now)?))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Content-Type", "application/json")
            .send(
                serde_json::json!({
                    "repositories": [self.repository],
                    "permissions": scope.permissions(),
                })
                .to_string(),
            )
            .context("GitHub refused the App's token request")?;
        let body: serde_json::Value = serde_json::from_slice(
            &response
                .body_mut()
                .read_to_vec()
                .context("could not read GitHub's token response")?,
        )
        .context("GitHub's token response is not JSON")?;
        body.get("token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .context("GitHub's token response has no token")
    }
}

/// An RSA private key from PEM: PKCS#1, as GitHub issues App keys, or PKCS#8.
fn parse_key(pem: &str) -> anyhow::Result<RsaKeyPair> {
    let label = pem
        .lines()
        .find_map(|line| line.strip_prefix("-----BEGIN ")?.strip_suffix("-----"))
        .context("no PEM block")?;
    let body: String = pem
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN "))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END "))
        .collect();
    let der = STANDARD
        .decode(body.trim())
        .context("the PEM body is not base64")?;
    match label {
        "RSA PRIVATE KEY" => {
            RsaKeyPair::from_der(&der).map_err(|error| anyhow!("not a PKCS#1 RSA key: {error}"))
        }
        "PRIVATE KEY" => {
            RsaKeyPair::from_pkcs8(&der).map_err(|error| anyhow!("not a PKCS#8 RSA key: {error}"))
        }
        other => bail!("a {other} block is not an RSA private key"),
    }
}

/// Answers git's credential-helper protocol: for `get` on `https://github.com`, an App token
/// from `mint`; for other hosts and for `store` and `erase`, nothing.
pub(crate) fn credential(
    operation: &str,
    request: &str,
    out: &mut impl Write,
    mint: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let field = |name: &str| {
        request.lines().find_map(|line| {
            line.split_once('=')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value)
        })
    };
    if field("protocol") != Some("https") || field("host") != Some("github.com") {
        return Ok(());
    }
    let token = mint()?;
    writeln!(out, "username=x-access-token")?;
    writeln!(out, "password={token}")?;
    Ok(())
}

/// `cargo xtask gh`: runs `program` (gh) with `args` as the App, with a token from `mint` for
/// this run, revoked through `agent` at `api` when it ends.
pub(crate) fn run_gh(
    mint: impl FnOnce() -> anyhow::Result<String>,
    agent: &ureq::Agent,
    api: &str,
    program: &Path,
    args: &[OsString],
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let token = mint()?;
    let code = gh(program, args, &token);
    if let Err(error) = revoke(agent, api, &token) {
        writeln!(
            out,
            "xtask gh: could not revoke the token; it expires within the hour: {error:#}"
        )?;
    }
    code
}

/// Revokes the installation `token` at `api`.
fn revoke(agent: &ureq::Agent, api: &str, token: &str) -> anyhow::Result<()> {
    agent
        .delete(&format!("{api}/installation/token"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("GitHub refused to revoke the token")?;
    Ok(())
}

/// Runs `gh` (the user's GitHub CLI, from `PATH`) with `args`, acting as the App through
/// `GH_TOKEN`. gh gets an empty configuration directory of its own: the user's `~/.config/gh/` is
/// unreadable in agent sessions, gh refuses to start without its configuration, and the App needs
/// nothing from it.
fn gh(program: &Path, args: &[OsString], token: &str) -> anyhow::Result<ExitCode> {
    let config = tempfile::tempdir().context("could not make a configuration directory for gh")?;
    let status = Command::new(program)
        .args(args)
        .env("GH_TOKEN", token)
        .env("GH_CONFIG_DIR", config.path())
        .env_remove("GITHUB_TOKEN")
        .status()
        .with_context(|| format!("could not run {}", program.display()))?;
    Ok(ExitCode::from(crate::tools::status_code(status)))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::{App, Scope, config_dir_from, credential, gh, parse_key, run_gh};
    use crate::testing::{fake_http, script};

    const KEY: &str = include_str!("../testdata/agent-app-test-key.pem");

    #[test]
    fn finds_the_config_in_this_environment() {
        assert!(
            super::config_dir().unwrap().ends_with("abc/agent-app"),
            "under the user's config directory"
        );
    }

    #[test]
    fn finds_the_config_under_home_whatever_xdg_says() {
        assert_eq!(
            config_dir_from(Some("/home/a".into())).unwrap(),
            Path::new("/home/a/.config/abc/agent-app"),
            "the path the sandbox denies, so XDG_CONFIG_HOME cannot move the key out of its reach"
        );
        assert!(config_dir_from(Some("".into())).is_err(), "an empty HOME");
        assert!(config_dir_from(None).is_err(), "no HOME");
    }

    #[test]
    fn names_each_scope_for_the_broker() {
        for scope in [Scope::Git, Scope::Gh] {
            assert_eq!(Scope::parse(scope.name()), Some(scope), "{scope:?}");
        }
        assert_eq!(Scope::Git.name(), "git");
        assert_eq!(Scope::Gh.name(), "gh");
        assert_eq!(Scope::parse("admin"), None, "no other scope");
    }

    /// An App configuration in a temporary directory, with the test key.
    fn app_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("app.json"),
            r#"{"client_id": "Iv23test", "installation_id": 42}"#,
        )
        .unwrap();
        let key = dir.path().join("private-key.pem");
        fs::write(&key, KEY).unwrap();
        fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        dir
    }

    #[test]
    fn refuses_a_key_others_can_read() {
        let dir = app_dir();
        let key = dir.path().join("private-key.pem");
        fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
        let error = format!("{:#}", App::load(dir.path()).unwrap_err());
        assert!(error.contains("readable by others (mode 640)"), "{error}");
        assert!(error.contains("chmod 600"), "{error}");
    }

    #[test]
    fn signs_a_jwt_github_can_verify() {
        let dir = app_dir();
        let app = App::load(dir.path()).unwrap();
        let debug = format!("{app:?}");
        assert!(
            debug.contains("Iv23test") && debug.contains("42"),
            "the IDs show: {debug}"
        );
        assert!(
            !debug.contains("key"),
            "the private key never does: {debug}"
        );
        let jwt = app.jwt(1_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "header, claims and signature");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({"iat": 999_940, "exp": 1_000_540, "iss": "Iv23test"}),
            "issued a minute early, nine minutes long, by the client ID"
        );
        let public = parse_key(KEY).unwrap();
        UnparsedPublicKey::new(
            &RSA_PKCS1_2048_8192_SHA256,
            ring::signature::KeyPair::public_key(&public).as_ref(),
        )
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .unwrap();
        assert!(
            !format!("{app:?}").contains("BEGIN"),
            "the key never prints"
        );
    }

    #[test]
    fn rejects_bad_keys_and_settings() {
        assert!(parse_key("not pem").is_err(), "no PEM block");
        assert!(
            parse_key("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n").is_err(),
            "a certificate"
        );
        assert!(
            parse_key("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n").is_err(),
            "a broken PKCS#8 key"
        );
        let dir = app_dir();
        fs::write(dir.path().join("app.json"), "{}").unwrap();
        let error = format!("{:#}", App::load(dir.path()).unwrap_err());
        assert!(
            error.contains("needs client_id and installation_id"),
            "{error}"
        );
    }

    #[test]
    fn requests_an_installation_token() {
        let (agent, server) = fake_http(
            ureq::Agent::config_builder(),
            vec![(
                201,
                r#"{"token": "ghs_test", "expires_at": "2026-11-01T11:00:00Z"}"#,
            )],
        );
        let dir = app_dir();
        let token = App::load(dir.path())
            .unwrap()
            .token(&agent, "https://api.github.test", Scope::Git)
            .unwrap();
        assert_eq!(token, "ghs_test", "the token from the response");
        let request = server.taken().remove(0);
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "repositories": ["abc"],
                "permissions": {"contents": "write", "workflows": "write"}
            }),
            "one repository, and only what git needs"
        );
        assert!(
            request.starts_with("POST /app/installations/42/access_tokens HTTP/1.1"),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer ey"),
            "a JWT: {request}"
        );
    }

    #[test]
    fn answers_the_credential_protocol() {
        let mut out = Vec::new();
        credential(
            "get",
            "protocol=https\nhost=github.com\npath=getabc-dev/abc.git\n",
            &mut out,
            || Ok("ghs_test".to_owned()),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "username=x-access-token\npassword=ghs_test\n",
            "git reads these two fields"
        );
        for (operation, request) in [
            ("get", "protocol=https\nhost=gitlab.com\n"),
            ("get", "protocol=http\nhost=github.com\n"),
            ("store", "protocol=https\nhost=github.com\npassword=x\n"),
            ("erase", "protocol=https\nhost=github.com\n"),
        ] {
            let mut silent = Vec::new();
            credential(operation, request, &mut silent, || {
                panic!("no token is minted for {operation} {request}")
            })
            .unwrap();
            assert!(silent.is_empty(), "{operation} {request}");
        }
    }

    #[test]
    fn runs_gh_with_the_token_in_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("gh");
        fs::write(
            &fake,
            "#!/bin/sh\n[ \"$GH_TOKEN\" = ghs_test ] && [ -z \"$GITHUB_TOKEN\" ] && [ \"$1\" = pr ] || exit 3\n[ -d \"$GH_CONFIG_DIR\" ] && [ -z \"$(ls -A \"$GH_CONFIG_DIR\")\" ] || exit 5\n[ \"$GH_CONFIG_DIR\" != \"$HOME/.config/gh\" ] || exit 6\n",
        )
        .unwrap();
        fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            gh(&fake, &["issue".into()], "ghs_test").unwrap(),
            std::process::ExitCode::from(3),
            "gh's own exit code comes back"
        );
        let code = gh(&fake, &["pr".into(), "create".into()], "ghs_test").unwrap();
        assert_eq!(
            code,
            std::process::ExitCode::SUCCESS,
            "gh saw the App's token, its arguments and an empty configuration of its own"
        );
    }

    #[test]
    fn scopes_each_use_to_what_it_needs() {
        let gh_permissions = Scope::Gh.permissions();
        assert_eq!(
            gh_permissions["pull_requests"], "write",
            "gh opens pull requests"
        );
        assert!(
            gh_permissions.get("workflows").is_none(),
            "gh does not edit workflows"
        );
        assert!(
            Scope::Git.permissions().get("pull_requests").is_none(),
            "git does not touch pull requests"
        );
    }

    #[test]
    fn runs_gh_with_a_token_it_revokes_afterwards() {
        let (agent, server) = fake_http(ureq::Agent::config_builder(), vec![(204, "")]);
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("gh");
        script(&fake, "[ \"$GH_TOKEN\" = ghs_test ] || exit 3\nexit 4\n");
        let mut out = Vec::new();
        let code = run_gh(
            || Ok("ghs_test".to_owned()),
            &agent,
            "https://api.github.test",
            &fake,
            &["pr".into(), "list".into()],
            &mut out,
        )
        .unwrap();
        assert_eq!(code, std::process::ExitCode::from(4), "gh's exit code");
        assert!(out.is_empty(), "no warning");
        let requests = server.taken();
        assert_eq!(requests.len(), 1, "only the revocation: {requests:?}");
        assert!(
            requests[0].starts_with("DELETE /installation/token HTTP/1.1"),
            "{requests:?}"
        );
        assert!(
            requests[0].contains("authorization: Bearer ghs_test"),
            "revoked with itself: {requests:?}"
        );

        let (refusing, refusing_server) =
            fake_http(ureq::Agent::config_builder(), vec![(500, "{}")]);
        let mut warned = Vec::new();
        run_gh(
            || Ok("ghs_test".to_owned()),
            &refusing,
            "https://api.github.test",
            &fake,
            &[],
            &mut warned,
        )
        .unwrap();
        assert_eq!(refusing_server.taken().len(), 1, "the revocation was tried");
        assert!(
            String::from_utf8(warned)
                .unwrap()
                .contains("could not revoke the token"),
            "a failed revocation is reported"
        );

        let error = run_gh(
            || anyhow::bail!("the broker is not running"),
            &agent,
            "https://api.github.test",
            &fake,
            &[],
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("the broker is not running"),
            "without a token gh never runs: {error}"
        );
    }
}
