//! Tunnel supervisor: one background task per tunnel owns the ssh child
//! process (and metering relay), reports state changes over a channel, and
//! reconnects with exponential backoff when a session drops.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::config::expand_tilde;
use crate::relay::{self, Counters, ListenSpec, Relay, SharedCounters, UpstreamSpec};
use crate::tunnel::{Endpoint, TunnelKind, TunnelSpec, TunnelState};

const CONNECT_GRACE: Duration = Duration::from_millis(1500);
const STABLE_UPTIME: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

pub struct Event {
    pub name: String,
    pub kind: EventKind,
}

pub enum EventKind {
    State(TunnelState),
    /// Fresh counters for a new session; totals restart from zero.
    Counters(SharedCounters),
    Log(String),
}

pub struct Manager {
    tx: mpsc::UnboundedSender<Event>,
    tunnels: HashMap<String, Handle>,
}

struct Handle {
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Manager {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx, tunnels: HashMap::new() }, rx)
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.tunnels
            .get(name)
            .is_some_and(|handle| !handle.task.is_finished())
    }

    pub fn connect(&mut self, spec: TunnelSpec) -> Result<(), String> {
        if self.is_running(&spec.name) {
            return Err(format!("`{}` is already running", spec.name));
        }
        let (cancel, cancel_rx) = watch::channel(false);
        let name = spec.name.clone();
        let task = tokio::spawn(supervise(spec, self.tx.clone(), cancel_rx));
        self.tunnels.insert(name, Handle { cancel, task });
        Ok(())
    }

    /// Ask a running tunnel to stop. Returns false when nothing was running.
    pub fn disconnect(&mut self, name: &str) -> bool {
        match self.tunnels.get(name) {
            Some(handle) if !handle.task.is_finished() => {
                let _ = handle.cancel.send(true);
                true
            }
            _ => false,
        }
    }

    pub fn running_names(&self) -> Vec<String> {
        self.tunnels
            .iter()
            .filter(|(_, handle)| !handle.task.is_finished())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Best-effort removal of a leftover unix socket on the remote host.
    pub fn clean_remote(&self, spec: &TunnelSpec) -> Result<(), String> {
        if self.is_running(&spec.name) {
            return Err("disconnect the tunnel before cleaning its socket".to_string());
        }
        let Some(path) = spec.remote_unix_socket() else {
            return Err("no remote unix socket to clean".to_string());
        };
        let path = path.to_string();
        let host = spec.host.clone();
        let name = spec.name.clone();
        let args = remote_rm_args(spec, &path);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = Command::new("ssh")
                .args(&args)
                .stdin(Stdio::null())
                .output()
                .await;
            let message = match result {
                Ok(output) if output.status.success() => {
                    format!("removed remote socket {path} on {host}")
                }
                Ok(output) => format!(
                    "remote socket cleanup failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                Err(error) => format!("remote socket cleanup failed: {error}"),
            };
            let _ = tx.send(Event { name, kind: EventKind::Log(message) });
        });
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        for handle in self.tunnels.values() {
            let _ = handle.cancel.send(true);
        }
        for (_, handle) in self.tunnels.drain() {
            let _ = tokio::time::timeout(Duration::from_secs(3), handle.task).await;
        }
    }
}

/// `ssh <host> rm -f -- <path>`, honoring the tunnel's extra ssh arguments.
fn remote_rm_args(spec: &TunnelSpec, path: &str) -> Vec<String> {
    let mut args: Vec<String> = ["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
        .into_iter()
        .map(String::from)
        .collect();
    args.extend(spec.extra_ssh_args.iter().cloned());
    args.extend([
        spec.host.clone(),
        "rm".to_string(),
        "-f".to_string(),
        "--".to_string(),
        path.to_string(),
    ]);
    args
}

/// Fire-and-forget removal of the remote socket after a session that had
/// bound it. sshd only unlinks these itself when the server's own
/// `StreamLocalBindUnlink` is enabled, which is not the default.
fn spawn_remote_cleanup(spec: &TunnelSpec, tx: &mpsc::UnboundedSender<Event>) {
    let Some(path) = spec.remote_unix_socket() else {
        return;
    };
    let args = remote_rm_args(spec, path);
    let name = spec.name.clone();
    let path = path.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = Command::new("ssh").args(&args).stdin(Stdio::null()).output().await;
        if matches!(&result, Ok(output) if output.status.success()) {
            let _ = tx.send(Event {
                name,
                kind: EventKind::Log(format!("removed remote socket {path}")),
            });
        }
    });
}

fn send_state(tx: &mpsc::UnboundedSender<Event>, name: &str, state: TunnelState) {
    let _ = tx.send(Event {
        name: name.to_string(),
        kind: EventKind::State(state),
    });
}

fn send_log(tx: &mpsc::UnboundedSender<Event>, name: &str, line: String) {
    let _ = tx.send(Event {
        name: name.to_string(),
        kind: EventKind::Log(line),
    });
}

/// Resolves once the cancel flag flips to true (or the manager is gone).
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn supervise(
    spec: TunnelSpec,
    tx: mpsc::UnboundedSender<Event>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut attempt: u32 = 0;
    let mut consecutive_failures: u32 = 0;

    loop {
        if *cancel.borrow() {
            send_state(&tx, &spec.name, TunnelState::Idle);
            return;
        }
        send_state(&tx, &spec.name, TunnelState::Connecting);

        match run_once(&spec, &tx, &mut cancel).await {
            RunOutcome::Cancelled => {
                send_state(&tx, &spec.name, TunnelState::Idle);
                return;
            }
            RunOutcome::Exited { reason, connected, uptime } => {
                send_log(&tx, &spec.name, format!("session ended: {reason}"));
                if connected {
                    consecutive_failures = 0;
                    if uptime >= STABLE_UPTIME {
                        attempt = 0;
                    }
                } else {
                    consecutive_failures += 1;
                }

                if !spec.auto_reconnect {
                    let state = if connected {
                        TunnelState::Idle
                    } else {
                        TunnelState::Failed { reason }
                    };
                    send_state(&tx, &spec.name, state);
                    return;
                }
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    send_state(
                        &tx,
                        &spec.name,
                        TunnelState::Failed {
                            reason: format!(
                                "giving up after {consecutive_failures} failed attempts: {reason}"
                            ),
                        },
                    );
                    return;
                }

                attempt += 1;
                let delay = backoff(attempt);
                send_state(
                    &tx,
                    &spec.name,
                    TunnelState::Reconnecting { attempt, retry_at: Instant::now() + delay },
                );
                tokio::select! {
                    _ = sleep(delay) => {}
                    _ = cancelled(&mut cancel) => {
                        send_state(&tx, &spec.name, TunnelState::Idle);
                        return;
                    }
                }
            }
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(2u64.pow(attempt.min(5)).min(30))
}

enum RunOutcome {
    Cancelled,
    Exited {
        reason: String,
        connected: bool,
        uptime: Duration,
    },
}

fn exited(reason: impl Into<String>) -> RunOutcome {
    RunOutcome::Exited {
        reason: reason.into(),
        connected: false,
        uptime: Duration::ZERO,
    }
}

async fn run_once(
    spec: &TunnelSpec,
    tx: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
) -> RunOutcome {
    let mut relay: Option<Relay> = None;
    let mut relay_port: Option<u16> = None;

    if spec.meter {
        let port = match relay::pick_free_port().await {
            Ok(port) => port,
            Err(error) => return exited(format!("no free port for metering relay: {error}")),
        };
        let (listen, upstream) = match relay_endpoints(spec, port) {
            Ok(pair) => pair,
            Err(error) => return exited(error),
        };
        let counters: SharedCounters = Arc::new(Counters::default());
        match Relay::start(listen, upstream, counters.clone()).await {
            Ok(started) => {
                relay = Some(started);
                relay_port = Some(port);
            }
            Err(error) => return exited(format!("metering relay failed to bind: {error}")),
        }
        let _ = tx.send(Event {
            name: spec.name.clone(),
            kind: EventKind::Counters(counters),
        });
    } else if let Some(path) = spec.local_unix_socket() {
        // ssh will bind this socket itself; clear any stale file first.
        if let Err(error) = relay::remove_stale_socket(&expand_tilde(path)) {
            return exited(error.to_string());
        }
    }

    // For -R unix sockets the bind happens on the remote, and sshd only
    // unlinks a stale socket file when the *server's* StreamLocalBindUnlink
    // is enabled (off by default). Clear it ourselves before binding, or
    // every reconnect fails with "remote port forwarding failed".
    if let Some(path) = spec.remote_unix_socket() {
        let rm = Command::new("ssh")
            .args(remote_rm_args(spec, path))
            .stdin(Stdio::null())
            .output();
        tokio::select! {
            result = rm => match result {
                Ok(output) if output.status.success() => {
                    send_log(tx, &spec.name, format!("cleared remote socket {path}"));
                }
                Ok(output) => {
                    // Bind may still succeed (e.g. no stale file); just log.
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    send_log(tx, &spec.name, format!(
                        "pre-bind socket cleanup failed: {}",
                        stderr.trim().lines().last().unwrap_or("unknown error"),
                    ));
                }
                Err(error) => {
                    send_log(tx, &spec.name, format!("pre-bind socket cleanup failed: {error}"));
                }
            },
            _ = cancelled(cancel) => return RunOutcome::Cancelled,
        }
    }

    let args = spec.ssh_args(relay_port);
    send_log(tx, &spec.name, format!("$ ssh {}", args.join(" ")));

    let mut child = match Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return exited(format!("failed to spawn ssh: {error}")),
    };

    let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        let tx = tx.clone();
        let name = spec.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut tail = tail.lock().unwrap();
                    tail.push(line.clone());
                    if tail.len() > 20 {
                        tail.remove(0);
                    }
                }
                send_log(&tx, &name, line);
            }
        });
    }

    let started = Instant::now();

    // ExitOnForwardFailure=yes means a child that survives the grace period
    // has authenticated and bound its forwardings.
    let grace = sleep(CONNECT_GRACE);
    tokio::pin!(grace);
    tokio::select! {
        status = child.wait() => {
            cleanup_after_exit(spec, relay.take(), tx);
            return RunOutcome::Exited {
                reason: exit_reason(status, &stderr_tail),
                connected: false,
                uptime: started.elapsed(),
            };
        }
        _ = cancelled(cancel) => {
            let _ = child.kill().await;
            cleanup_after_exit(spec, relay.take(), tx);
            return RunOutcome::Cancelled;
        }
        _ = &mut grace => {}
    }

    send_state(tx, &spec.name, TunnelState::Connected { since: started });

    tokio::select! {
        status = child.wait() => {
            cleanup_after_exit(spec, relay.take(), tx);
            RunOutcome::Exited {
                reason: exit_reason(status, &stderr_tail),
                connected: true,
                uptime: started.elapsed(),
            }
        }
        _ = cancelled(cancel) => {
            let _ = child.kill().await;
            cleanup_after_exit(spec, relay.take(), tx);
            RunOutcome::Cancelled
        }
    }
}

/// Where the metering relay listens and connects for each tunnel kind.
fn relay_endpoints(spec: &TunnelSpec, port: u16) -> Result<(ListenSpec, UpstreamSpec), String> {
    match spec.kind {
        // Relay owns the user-facing endpoint; ssh listens on a loopback port.
        TunnelKind::Local | TunnelKind::Dynamic => {
            let listen = match &spec.listen {
                Endpoint::Tcp { host, port } => ListenSpec::Tcp(format!(
                    "{}:{port}",
                    host.clone().unwrap_or_else(|| "127.0.0.1".to_string())
                )),
                Endpoint::Unix { path } => ListenSpec::Unix(expand_tilde(path)),
            };
            Ok((listen, UpstreamSpec::Tcp(format!("127.0.0.1:{port}"))))
        }
        // ssh delivers remote-originated traffic to the relay, which forwards
        // to the real local target.
        TunnelKind::Remote => {
            let target = spec
                .target
                .as_ref()
                .ok_or_else(|| "remote tunnel needs a target".to_string())?;
            let upstream = match target {
                Endpoint::Tcp { host, port } => UpstreamSpec::Tcp(format!(
                    "{}:{port}",
                    host.clone().unwrap_or_else(|| "127.0.0.1".to_string())
                )),
                Endpoint::Unix { path } => UpstreamSpec::Unix(expand_tilde(path)),
            };
            Ok((ListenSpec::Tcp(format!("127.0.0.1:{port}")), upstream))
        }
    }
}

fn cleanup_after_exit(spec: &TunnelSpec, relay: Option<Relay>, tx: &mpsc::UnboundedSender<Event>) {
    // Dropping the relay unlinks any unix socket it owned.
    drop(relay);
    if !spec.meter
        && let Some(path) = spec.local_unix_socket() {
            let _ = relay::remove_stale_socket(&expand_tilde(path));
        }
    // sshd leaves -R unix sockets behind on the remote host.
    spawn_remote_cleanup(spec, tx);
}

fn exit_reason(
    status: std::io::Result<std::process::ExitStatus>,
    stderr_tail: &Arc<Mutex<Vec<String>>>,
) -> String {
    let last_line = stderr_tail.lock().unwrap().last().cloned();
    match (last_line, status) {
        (Some(line), _) => line,
        (None, Ok(status)) => format!("ssh exited ({status})"),
        (None, Err(error)) => format!("ssh wait failed: {error}"),
    }
}
