use std::fmt;
use std::str::FromStr;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::expand_tilde;

/// One side of a forwarding: a TCP address or a unix domain socket path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Endpoint {
    Tcp { host: Option<String>, port: u16 },
    Unix { path: String },
}

impl Endpoint {
    pub fn is_unix(&self) -> bool {
        matches!(self, Endpoint::Unix { .. })
    }

    /// The endpoint as an ssh forwarding argument fragment. Tilde paths are
    /// expanded only when the socket lives on this machine; remote-side paths
    /// are passed through untouched for the remote shell/sshd to resolve.
    pub fn arg(&self, expand_local_path: bool) -> String {
        match self {
            Endpoint::Unix { path } if expand_local_path => {
                expand_tilde(path).to_string_lossy().into_owned()
            }
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Tcp { host: Some(host), port } => write!(f, "{host}:{port}"),
            Endpoint::Tcp { host: None, port } => write!(f, "{port}"),
            Endpoint::Unix { path } => write!(f, "{path}"),
        }
    }
}

impl FromStr for Endpoint {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        if input.is_empty() {
            return Err("endpoint is empty".to_string());
        }
        if input.contains('/') || input.starts_with('~') {
            return Ok(Endpoint::Unix {
                path: input.to_string(),
            });
        }
        if let Some((host, port)) = input.rsplit_once(':') {
            let port = port
                .parse()
                .map_err(|_| format!("invalid port in `{input}`"))?;
            let host = (!host.is_empty()).then(|| host.to_string());
            return Ok(Endpoint::Tcp { host, port });
        }
        let port = input.parse().map_err(|_| {
            format!("`{input}` is not a port, host:port, or unix socket path")
        })?;
        Ok(Endpoint::Tcp { host: None, port })
    }
}

impl TryFrom<String> for Endpoint {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Endpoint> for String {
    fn from(endpoint: Endpoint) -> Self {
        endpoint.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelKind {
    /// `ssh -L listen:target host` — local listener, traffic goes out.
    #[default]
    Local,
    /// `ssh -R listen:target host` — remote listener, traffic comes in.
    Remote,
    /// `ssh -D listen host` — local SOCKS proxy.
    Dynamic,
}

impl TunnelKind {
    pub fn label(self) -> &'static str {
        match self {
            TunnelKind::Local => "local (-L)",
            TunnelKind::Remote => "remote (-R)",
            TunnelKind::Dynamic => "dynamic (-D)",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            TunnelKind::Local => "L",
            TunnelKind::Remote => "R",
            TunnelKind::Dynamic => "D",
        }
    }

    pub fn next(self) -> Self {
        match self {
            TunnelKind::Local => TunnelKind::Remote,
            TunnelKind::Remote => TunnelKind::Dynamic,
            TunnelKind::Dynamic => TunnelKind::Local,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelSpec {
    pub name: String,
    /// Host alias resolved through ~/.ssh/config; authentication is ssh's job.
    pub host: String,
    #[serde(default)]
    pub kind: TunnelKind,
    pub listen: Endpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Endpoint>,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    /// Route traffic through an in-process counting relay for live stats.
    #[serde(default = "default_true")]
    pub meter: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_ssh_args: Vec<String>,
}

impl TunnelSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name is required".to_string());
        }
        if self.host.trim().is_empty() {
            return Err("ssh host is required".to_string());
        }
        match self.kind {
            TunnelKind::Dynamic => {
                if self.listen.is_unix() {
                    return Err("dynamic (-D) tunnels need a TCP listen endpoint".to_string());
                }
            }
            TunnelKind::Local | TunnelKind::Remote => {
                if self.target.is_none() {
                    return Err("target endpoint is required".to_string());
                }
            }
        }
        Ok(())
    }

    /// Full argument list for the ssh child process. When `relay_port` is set
    /// the metering relay owns the user-facing local endpoint and ssh is wired
    /// to a loopback port instead.
    pub fn ssh_args(&self, relay_port: Option<u16>) -> Vec<String> {
        let mut args: Vec<String> = [
            "-N",
            "-T",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "StreamLocalBindUnlink=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "BatchMode=yes",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        match self.kind {
            TunnelKind::Local => {
                let listen = relay_port
                    .map(|port| format!("127.0.0.1:{port}"))
                    .unwrap_or_else(|| self.listen.arg(true));
                let target = self
                    .target
                    .as_ref()
                    .map(|target| target.arg(false))
                    .unwrap_or_default();
                args.push("-L".to_string());
                args.push(format!("{listen}:{target}"));
            }
            TunnelKind::Remote => {
                let listen = self.listen.arg(false);
                let target = relay_port
                    .map(|port| format!("127.0.0.1:{port}"))
                    .unwrap_or_else(|| {
                        self.target
                            .as_ref()
                            .map(|target| target.arg(true))
                            .unwrap_or_default()
                    });
                args.push("-R".to_string());
                args.push(format!("{listen}:{target}"));
            }
            TunnelKind::Dynamic => {
                let listen = relay_port
                    .map(|port| format!("127.0.0.1:{port}"))
                    .unwrap_or_else(|| self.listen.arg(true));
                args.push("-D".to_string());
                args.push(listen);
            }
        }

        args.extend(self.extra_ssh_args.iter().cloned());
        args.push(self.host.clone());
        args
    }

    /// One-line route summary, entrance first.
    pub fn route(&self) -> String {
        match self.kind {
            TunnelKind::Local => format!(
                "{} → {} → {}",
                self.listen,
                self.host,
                self.target.as_ref().map(ToString::to_string).unwrap_or_default(),
            ),
            TunnelKind::Remote => format!(
                "{}:{} → local {}",
                self.host,
                self.listen,
                self.target.as_ref().map(ToString::to_string).unwrap_or_default(),
            ),
            TunnelKind::Dynamic => format!("{} → {} → SOCKS", self.listen, self.host),
        }
    }

    /// The unix socket path on this machine that a session leaves behind, if any.
    pub fn local_unix_socket(&self) -> Option<&str> {
        match (self.kind, &self.listen) {
            (TunnelKind::Local | TunnelKind::Dynamic, Endpoint::Unix { path }) => Some(path),
            _ => None,
        }
    }

    /// The unix socket path on the remote host that a session leaves behind, if any.
    pub fn remote_unix_socket(&self) -> Option<&str> {
        match (self.kind, &self.listen) {
            (TunnelKind::Remote, Endpoint::Unix { path }) => Some(path),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum TunnelState {
    #[default]
    Idle,
    Connecting,
    Connected {
        since: Instant,
    },
    Reconnecting {
        attempt: u32,
        retry_at: Instant,
    },
    Failed {
        reason: String,
    },
}

impl TunnelState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TunnelState::Connecting | TunnelState::Connected { .. } | TunnelState::Reconnecting { .. }
        )
    }

    pub fn glyph(&self) -> &'static str {
        match self {
            TunnelState::Idle => "○",
            TunnelState::Connecting => "◌",
            TunnelState::Connected { .. } => "●",
            TunnelState::Reconnecting { .. } => "↻",
            TunnelState::Failed { .. } => "✕",
        }
    }

    pub fn word(&self) -> &'static str {
        match self {
            TunnelState::Idle => "idle",
            TunnelState::Connecting => "connecting",
            TunnelState::Connected { .. } => "connected",
            TunnelState::Reconnecting { .. } => "reconnecting",
            TunnelState::Failed { .. } => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: TunnelKind, listen: &str, target: Option<&str>) -> TunnelSpec {
        TunnelSpec {
            name: "t".to_string(),
            host: "myhost".to_string(),
            kind,
            listen: listen.parse().unwrap(),
            target: target.map(|t| t.parse().unwrap()),
            auto_reconnect: true,
            meter: false,
            extra_ssh_args: vec![],
        }
    }

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            "5432".parse::<Endpoint>().unwrap(),
            Endpoint::Tcp { host: None, port: 5432 }
        );
        assert_eq!(
            "0.0.0.0:80".parse::<Endpoint>().unwrap(),
            Endpoint::Tcp { host: Some("0.0.0.0".to_string()), port: 80 }
        );
        assert_eq!(
            "/tmp/x.sock".parse::<Endpoint>().unwrap(),
            Endpoint::Unix { path: "/tmp/x.sock".to_string() }
        );
        assert_eq!(
            "~/run/x.sock".parse::<Endpoint>().unwrap(),
            Endpoint::Unix { path: "~/run/x.sock".to_string() }
        );
        assert!("".parse::<Endpoint>().is_err());
        assert!("host:notaport".parse::<Endpoint>().is_err());
    }

    #[test]
    fn endpoint_display_round_trips() {
        for text in ["5432", "0.0.0.0:80", "/tmp/x.sock"] {
            assert_eq!(text.parse::<Endpoint>().unwrap().to_string(), text);
        }
    }

    #[test]
    fn builds_local_forward_args() {
        let args = spec(TunnelKind::Local, "127.0.0.1:5433", Some("db:5432")).ssh_args(None);
        let forward = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[forward + 1], "127.0.0.1:5433:db:5432");
        assert_eq!(args.last().unwrap(), "myhost");
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(args.contains(&"StreamLocalBindUnlink=yes".to_string()));
    }

    #[test]
    fn builds_remote_forward_to_unix_socket() {
        let args = spec(TunnelKind::Remote, "/var/run/app.sock", Some("3000")).ssh_args(None);
        let forward = args.iter().position(|a| a == "-R").unwrap();
        assert_eq!(args[forward + 1], "/var/run/app.sock:3000");
    }

    #[test]
    fn metered_tunnel_wires_ssh_to_relay_port() {
        let args = spec(TunnelKind::Local, "127.0.0.1:5433", Some("db:5432")).ssh_args(Some(40001));
        let forward = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[forward + 1], "127.0.0.1:40001:db:5432");

        let args = spec(TunnelKind::Remote, "9000", Some("3000")).ssh_args(Some(40002));
        let forward = args.iter().position(|a| a == "-R").unwrap();
        assert_eq!(args[forward + 1], "9000:127.0.0.1:40002");
    }

    #[test]
    fn dynamic_requires_tcp_listen() {
        assert!(spec(TunnelKind::Dynamic, "/tmp/x.sock", None).validate().is_err());
        assert!(spec(TunnelKind::Dynamic, "1080", None).validate().is_ok());
    }
}
