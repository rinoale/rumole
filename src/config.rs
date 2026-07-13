use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tunnel::TunnelSpec;

const CONFIG_HEADER: &str = "\
# rumole tunnel definitions.
#
# Each [[tunnel]] entry describes one ssh forwarding:
#   name           unique display name
#   host           Host alias from ~/.ssh/config (auth is ssh's job)
#   kind           \"local\" (-L) | \"remote\" (-R) | \"dynamic\" (-D)
#   listen         entrance endpoint: \"5433\", \"0.0.0.0:5433\", or \"/path/to.sock\"
#   target         exit endpoint (omit for dynamic)
#   auto_reconnect reconnect with backoff when the session drops (default true)
#   meter          count traffic through an in-process relay (default true)
#   extra_ssh_args extra arguments appended to the ssh command
";

const SAMPLE: &str = "\
# [[tunnel]]
# name = \"example-db\"
# host = \"myserver\"
# kind = \"local\"
# listen = \"127.0.0.1:5433\"
# target = \"127.0.0.1:5432\"
#
# [[tunnel]]
# name = \"expose-dev\"
# host = \"myserver\"
# kind = \"remote\"
# listen = \"/var/run/dev.sock\"
# target = \"127.0.0.1:3000\"
";

pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~"
        && let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    PathBuf::from(path)
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty() {
            return PathBuf::from(dir).join("rumole");
        }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("rumole")
}

pub fn tunnels_path() -> PathBuf {
    config_dir().join("tunnels.toml")
}

pub fn history_path() -> PathBuf {
    config_dir().join("history.jsonl")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, rename = "tunnel")]
    pub tunnels: Vec<TunnelSpec>,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let path = tunnels_path();
        if !path.exists() {
            let config = Self::default();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&path, format!("{CONFIG_HEADER}\n{SAMPLE}"));
            return Ok(config);
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn save(&self) -> Result<(), String> {
        let path = tunnels_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|error| format!("cannot serialize config: {error}"))?;
        fs::write(&path, format!("{CONFIG_HEADER}\n{body}"))
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }
}

/// Host aliases from ~/.ssh/config, for suggestions in the tunnel form.
/// Wildcard and negated patterns are skipped.
pub fn ssh_config_hosts() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    let path = Path::new(&home).join(".ssh").join("config");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("host ")
            .map(|_| line[4..].trim_start())
        {
            for token in rest.split_whitespace() {
                if !token.contains(['*', '?', '!']) {
                    hosts.push(token.to_string());
                }
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::{Endpoint, TunnelKind};

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config {
            tunnels: vec![TunnelSpec {
                name: "db".to_string(),
                host: "myserver".to_string(),
                kind: TunnelKind::Local,
                listen: "127.0.0.1:5433".parse().unwrap(),
                target: Some("10.0.0.5:5432".parse().unwrap()),
                auto_reconnect: true,
                meter: true,
                extra_ssh_args: vec![],
            }],
        };
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.tunnels, config.tunnels);
    }

    #[test]
    fn parses_unix_socket_tunnel_from_toml() {
        let text = r#"
            [[tunnel]]
            name = "sock"
            host = "myserver"
            kind = "remote"
            listen = "/var/run/app.sock"
            target = "3000"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        let spec = &config.tunnels[0];
        assert_eq!(spec.kind, TunnelKind::Remote);
        assert_eq!(spec.listen, Endpoint::Unix { path: "/var/run/app.sock".to_string() });
        assert!(spec.auto_reconnect);
        assert!(spec.meter);
    }
}
