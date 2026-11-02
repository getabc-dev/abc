//! The hosts agent VMs may reach, from `xtask/vm/egress-allow.txt`.
//!
//! One host name per line, or `*.` and a domain for all of its subdomains; `#` starts a comment.
//! microsandbox enforces the list on the host: everything else, the host machine, the LAN and
//! other VMs included, is refused.

use anyhow::{bail, ensure};

/// The allowlist, relative to the repository root.
pub(crate) const FILE: &str = "xtask/vm/egress-allow.txt";

/// The hosts in the allowlist `text`, checked.
pub(crate) fn parse(text: &str) -> anyhow::Result<Vec<String>> {
    let mut hosts = Vec::new();
    for (number, line) in (1_usize..).zip(text.lines()) {
        let host = line.split('#').next().unwrap_or_default().trim();
        if host.is_empty() {
            continue;
        }
        let name = host.strip_prefix("*.").unwrap_or(host);
        let labels: Vec<&str> = name.split('.').collect();
        let valid = labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
        if !valid || labels.len() < 2 {
            bail!("line {number}: {host:?} is not a lowercase host name within a domain");
        }
        hosts.push(host.to_owned());
    }
    ensure!(!hosts.is_empty(), "the egress allowlist names no host");
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reads_hosts_and_wildcards() {
        assert_eq!(
            parse("# comment\ngithub.com\n\n*.githubusercontent.com  # assets\n").unwrap(),
            ["github.com", "*.githubusercontent.com"]
        );
    }

    #[test]
    fn the_checked_in_list_is_valid() {
        let text = std::fs::read_to_string(crate::repo_root().unwrap().join(super::FILE)).unwrap();
        let hosts = parse(&text).unwrap();
        for needed in [
            "github.com",
            "api.github.com",
            "index.crates.io",
            "api.anthropic.com",
        ] {
            assert!(hosts.iter().any(|host| host == needed), "{needed}");
        }
    }

    #[test]
    fn refuses_what_is_not_a_host() {
        for bad in [
            "GitHub.com",
            "github",
            "*.com",
            "*",
            "https://github.com",
            "-a.com",
            "a-.com",
            "a..com",
            "a_b.com",
            &format!("{}.com", "a".repeat(64)),
        ] {
            let error = format!("{:#}", parse(&format!("ok.com\n{bad}\n")).unwrap_err());
            assert!(error.starts_with("line 2: "), "{bad}: {error}");
        }
        assert!(parse("# nothing\n").is_err(), "an empty list");
        assert_eq!(parse(&format!("{}.com", "a".repeat(63))).unwrap().len(), 1);
    }
}
