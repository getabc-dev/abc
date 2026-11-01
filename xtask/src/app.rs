//! Acting as the abc agent GitHub App (D3).
//!
//! Agents push, comment and open pull requests as the App, with installation tokens that expire
//! in an hour, never with the user's own credentials. Signing a token request takes the App's
//! client ID, its installation ID and its private key, kept outside the repository in
//! `~/.config/abc/agent-app/` (`app.json` and `private-key.pem`). A token is minted for every use
//! and goes only where it is needed: to git through the credential-helper protocol, or to `gh` in
//! `GH_TOKEN`. It is never printed or logged.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;

use crate::git;

/// GitHub's REST API.
pub(crate) const GITHUB_API: &str = "https://api.github.com";

/// The git config key that names credential helpers for github.com.
const HELPER_KEY: &str = "credential.https://github.com.helper";

/// The helper git runs from inside the repository.
const HELPER: &str = "!cargo xtask git-credential";

/// The App's settings in `app.json`.
#[derive(Deserialize)]
struct Settings {
    client_id: String,
    installation_id: u64,
}

/// The App, ready to sign token requests.
pub(crate) struct App {
    client_id: String,
    installation_id: u64,
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

/// Where the App's files live: `$XDG_CONFIG_HOME/abc/agent-app`, else `~/.config/abc/agent-app`.
pub(crate) fn config_dir() -> anyhow::Result<PathBuf> {
    config_dir_from(|name| std::env::var_os(name))
}

/// [`config_dir`], with the environment read through `lookup`.
fn config_dir_from(lookup: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    let base = match lookup("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(lookup("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("abc/agent-app"))
}

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
        let pem = fs::read_to_string(&key_path)
            .with_context(|| format!("could not read {}", key_path.display()))?;
        Ok(Self {
            client_id: settings.client_id,
            installation_id: settings.installation_id,
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

    /// A fresh installation token from the GitHub API at `api`, valid for an hour.
    pub(crate) fn token(&self, api: &str) -> anyhow::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("the clock is before 1970")?
            .as_secs();
        let url = format!(
            "{api}/app/installations/{}/access_tokens",
            self.installation_id
        );
        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.jwt(now)?))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send_empty()
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

/// Runs `gh` (the user's GitHub CLI, from `PATH`) with `args`, acting as the App through
/// `GH_TOKEN`.
pub(crate) fn gh(
    program: &Path,
    args: &[OsString],
    token: &str,
) -> anyhow::Result<ExitCode> {
    let status = Command::new(program)
        .args(args)
        .env("GH_TOKEN", token)
        .env_remove("GITHUB_TOKEN")
        .status()
        .with_context(|| format!("could not run {}", program.display()))?;
    Ok(ExitCode::from(crate::tools::status_code(status)))
}

/// Makes git in this repository ask the App for github.com credentials, ahead of and instead of
/// the user's own helpers, once the App's files exist in `dir`. An empty helper entry first
/// resets the helpers inherited from the user's and the system's git config.
pub(crate) fn configure_git(root: &Path, dir: &Path, out: &mut impl Write) -> anyhow::Result<()> {
    if !dir.join("app.json").is_file() {
        writeln!(
            out,
            "note     no agent App in {}; git and gh keep your own credentials",
            dir.display()
        )?;
        return Ok(());
    }
    let current =
        git::git(root, &["config", "--local", "--get-all", HELPER_KEY]).unwrap_or_default();
    if current.lines().eq(["", HELPER]) {
        writeln!(out, "ok       git uses the agent App for github.com")?;
        return Ok(());
    }
    git::git(
        root,
        &["config", "--local", "--replace-all", HELPER_KEY, ""],
    )?;
    git::git(root, &["config", "--local", "--add", HELPER_KEY, HELPER])?;
    let written = git::git(root, &["config", "--local", "--get-all", HELPER_KEY])?;
    ensure!(
        written.lines().eq(["", HELPER]),
        "git config did not take the agent App's credential helper"
    );
    writeln!(out, "set      git uses the agent App for github.com")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::Path;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::{
        App, HELPER, HELPER_KEY, config_dir_from, configure_git, credential, gh, parse_key,
    };
    use crate::testing::{git, repository};

    const KEY: &str = include_str!("../testdata/agent-app-test-key.pem");

    #[test]
    fn finds_the_config_under_xdg_or_home() {
        let lookup = |xdg: Option<&'static str>| {
            move |name: &str| match name {
                "XDG_CONFIG_HOME" => xdg.map(Into::into),
                "HOME" => Some("/home/a".into()),
                _ => None,
            }
        };
        assert_eq!(
            config_dir_from(lookup(Some("/xdg"))).unwrap(),
            Path::new("/xdg/abc/agent-app"),
            "XDG_CONFIG_HOME first"
        );
        assert_eq!(
            config_dir_from(lookup(Some(""))).unwrap(),
            Path::new("/home/a/.config/abc/agent-app"),
            "an empty XDG_CONFIG_HOME is unset"
        );
        assert_eq!(
            config_dir_from(lookup(None)).unwrap(),
            Path::new("/home/a/.config/abc/agent-app"),
            "else ~/.config"
        );
        assert!(config_dir_from(|_| None).is_err(), "no HOME");
    }

    /// An App configuration in a temporary directory, with the test key.
    fn app_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("app.json"),
            r#"{"client_id": "Iv23test", "installation_id": 42}"#,
        )
        .unwrap();
        fs::write(dir.path().join("private-key.pem"), KEY).unwrap();
        dir
    }

    #[test]
    fn signs_a_jwt_github_can_verify() {
        let dir = app_dir();
        let app = App::load(dir.path()).unwrap();
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            let body = r#"{"token": "ghs_test", "expires_at": "2026-11-01T11:00:00Z"}"#;
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        let dir = app_dir();
        let token = App::load(dir.path())
            .unwrap()
            .token(&format!("http://{address}"))
            .unwrap();
        let request = server.join().unwrap();
        assert_eq!(token, "ghs_test", "the token from the response");
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
            "#!/bin/sh\n[ \"$GH_TOKEN\" = ghs_test ] && [ -z \"$GITHUB_TOKEN\" ] && [ \"$1\" = pr ] || exit 3\n",
        )
        .unwrap();
        fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let code = gh(&fake, &["pr".into(), "create".into()], "ghs_test").unwrap();
        assert_eq!(
            code,
            std::process::ExitCode::SUCCESS,
            "gh saw the App's token and arguments"
        );
    }

    #[test]
    fn configures_git_only_when_the_app_is_set_up() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        let missing = temp.path().join("none");
        let mut out = Vec::new();
        configure_git(&repo, &missing, &mut out).unwrap();
        assert!(
            String::from_utf8(out).unwrap().starts_with("note "),
            "no App, no change"
        );

        let dir = app_dir();
        for _ in 0..2 {
            configure_git(&repo, dir.path(), &mut Vec::new()).unwrap();
        }
        assert_eq!(
            git(&repo, &["config", "--local", "--get-all", HELPER_KEY]),
            format!("\n{HELPER}\n"),
            "the reset entry, then the App's helper, set once"
        );
        assert!(
            Path::new(&repo).exists(),
            "the repository is untouched otherwise"
        );
    }
}
