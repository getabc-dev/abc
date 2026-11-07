//! `config.toml`: what setup learned and what the services run with.
//!
//! ```toml
//! format = 1
//! public_url = "https://abc.tail1234.ts.net"   # once abc's node has logged in
//! ops_addr = "127.0.0.1:7071"                  # abc's health and readiness
//!
//! [programs]
//! abc = "/opt/homebrew/opt/abc/bin/abc"
//! tailscaled = "/opt/homebrew/opt/abc/libexec/tailscaled"
//! tailscale = "/opt/homebrew/opt/abc/libexec/tailscale"
//! postgres = "/opt/homebrew/opt/abc/libexec/postgresql"   # Postgres 18's bin directory
//! ```
//!
//! Setup writes it the first time from where abc is installed ([`crate::programs`]), and reads
//! it every time after, so a path set here by hand wins. The services are written from it, so
//! after editing it, `abc server setup` (or `upgrade`) puts the change into effect.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use abc_protocol::url::PublicUrl;
use serde::{Deserialize, Serialize};

use crate::home::{private, write_private};
use crate::programs::Programs;
use crate::{HostError, ServerHome};

/// The format this build reads and writes. There are no migrations (S3): any other is refused.
const FORMAT: u32 = 1;

/// The ops address unless the configuration names another.
pub const DEFAULT_OPS_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7071));

/// abc server's configuration on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// [`FORMAT`].
    format: u32,
    /// The https URL Serve answers on, read from abc's node once it has logged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<PublicUrl>,
    /// Where abc server's health and readiness listen.
    pub ops_addr: SocketAddr,
    /// The programs the stack runs.
    pub programs: Programs,
}

impl Config {
    /// A configuration with no public URL yet.
    #[must_use]
    pub const fn new(programs: Programs) -> Self {
        Self {
            format: FORMAT,
            public_url: None,
            ops_addr: DEFAULT_OPS_ADDR,
            programs,
        }
    }

    /// The home's configuration, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// [`HostError::Config`] for one that does not parse or is of another format,
    /// [`HostError::Insecure`] for one others can read.
    pub fn load(home: &ServerHome) -> Result<Option<Self>, HostError> {
        let path = home.config();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(HostError::io(&path, source)),
        };
        private(&path)?;
        let config: Self = toml_edit::de::from_str(&text).map_err(|error| HostError::Config {
            path: path.clone(),
            reason: error.message().to_owned(),
        })?;
        if config.format != FORMAT {
            return Err(HostError::Config {
                path,
                reason: format!(
                    "format {} where this abc reads format {FORMAT}",
                    config.format
                ),
            });
        }
        Ok(Some(config))
    }

    /// The home's configuration, which a command other than setup needs.
    ///
    /// # Errors
    ///
    /// [`HostError::NotSetUp`] when there is none, and as [`Config::load`].
    pub fn require(home: &ServerHome) -> Result<Self, HostError> {
        Self::load(home)?.ok_or_else(|| HostError::NotSetUp {
            path: home.config(),
        })
    }

    /// Writes it, privately and atomically.
    ///
    /// # Errors
    ///
    /// [`HostError::Io`] or [`HostError::Insecure`].
    pub fn save(&self, home: &ServerHome) -> Result<(), HostError> {
        let path = home.config();
        let text = toml_edit::ser::to_string_pretty(self).map_err(|error| HostError::Config {
            path: path.clone(),
            reason: error.to_string(),
        })?;
        write_private(&path, &text).map(|_changed| ())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use abc_protocol::url::PublicUrl;

    use super::{Config, DEFAULT_OPS_ADDR};
    use crate::programs::Programs;
    use crate::{HostError, ServerHome};

    fn programs() -> Programs {
        Programs {
            abc: PathBuf::from("/opt/abc/bin/abc"),
            tailscaled: PathBuf::from("/opt/abc/libexec/tailscaled"),
            tailscale: PathBuf::from("/opt/abc/libexec/tailscale"),
            postgres: PathBuf::from("/opt/abc/libexec/postgresql"),
        }
    }

    fn home() -> (tempfile::TempDir, ServerHome) {
        let scratch = tempfile::tempdir().unwrap();
        let home = ServerHome::at(scratch.path().join("server"));
        home.create().unwrap();
        (scratch, home)
    }

    #[test]
    fn a_saved_configuration_loads_back_and_reads_as_documented() {
        let (_scratch, home) = home();
        assert_eq!(Config::load(&home).unwrap(), None, "none yet");
        let mut config = Config::new(programs());
        assert_eq!(config.ops_addr, DEFAULT_OPS_ADDR, "loopback 7071");
        config.save(&home).unwrap();
        assert_eq!(
            Config::load(&home).unwrap(),
            Some(config.clone()),
            "round trip"
        );
        config.public_url = Some(PublicUrl::parse("https://abc.tail1.ts.net").unwrap());
        config.save(&home).unwrap();
        let text = std::fs::read_to_string(home.config()).unwrap();
        assert_eq!(
            text,
            "format = 1\npublic_url = \"https://abc.tail1.ts.net\"\nops_addr = \"127.0.0.1:7071\"\n\n\
             [programs]\nabc = \"/opt/abc/bin/abc\"\ntailscaled = \"/opt/abc/libexec/tailscaled\"\n\
             tailscale = \"/opt/abc/libexec/tailscale\"\npostgres = \"/opt/abc/libexec/postgresql\"\n",
            "the documented shape"
        );
        assert_eq!(
            Config::require(&home).unwrap(),
            config,
            "required and there"
        );
    }

    #[test]
    fn a_missing_damaged_foreign_or_open_configuration_names_its_fix() {
        let (_scratch, home) = home();
        let missing = Config::require(&home).unwrap_err();
        assert!(
            missing.to_string().ends_with("run abc server setup"),
            "{missing}"
        );
        std::fs::write(home.config(), "format = ").unwrap();
        std::fs::set_permissions(home.config(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let damaged = Config::load(&home).unwrap_err();
        assert!(matches!(damaged, HostError::Config { .. }), "{damaged:?}");
        assert!(
            damaged
                .to_string()
                .contains("delete it and run abc server setup again"),
            "{damaged}"
        );
        let mut foreign = toml_edit::ser::to_string(&Config::new(programs())).unwrap();
        foreign = foreign.replace("format = 1", "format = 2");
        std::fs::write(home.config(), foreign).unwrap();
        let other = Config::load(&home).unwrap_err();
        assert!(
            other
                .to_string()
                .contains("format 2 where this abc reads format 1"),
            "{other}"
        );
        std::fs::set_permissions(home.config(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(Config::load(&home), Err(HostError::Insecure { .. })),
            "others may read it"
        );
        std::fs::remove_file(home.config()).unwrap();
        std::fs::create_dir(home.config()).unwrap();
        assert!(
            matches!(Config::load(&home), Err(HostError::Io { .. })),
            "one that cannot be read is an error, never absent"
        );
    }
}
