//! Tunnel supervisor: one background task per tunnel owns the ssh child
//! process (and metering relay), reports state changes over a channel, and
//! reconnects with exponential backoff when a session drops.

use std::collections::HashMap;
use std::future::{Future, pending};
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
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
        (
            Self {
                tx,
                tunnels: HashMap::new(),
            },
            rx,
        )
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
            let _ = tx.send(Event {
                name,
                kind: EventKind::Log(message),
            });
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
        let result = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .await;
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
            RunOutcome::Exited {
                reason,
                connected,
                uptime,
            } => {
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
                    TunnelState::Reconnecting {
                        attempt,
                        retry_at: Instant::now() + delay,
                    },
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
        let port = match spec.kind {
            // Bind once and keep ownership of the assigned port. Picking and
            // releasing a free port before rebinding races other sessions.
            TunnelKind::Remote => 0,
            TunnelKind::Local | TunnelKind::Dynamic => match relay::pick_free_port().await {
                Ok(port) => port,
                Err(error) => return exited(format!("no free port for metering relay: {error}")),
            },
        };
        let (listen, upstream) = match relay_endpoints(spec, port) {
            Ok(pair) => pair,
            Err(error) => return exited(error),
        };
        let counters: SharedCounters = Arc::new(Counters::default());
        match Relay::start(listen, upstream, counters.clone()).await {
            Ok(started) => {
                relay_port = Some(match spec.kind {
                    TunnelKind::Remote => {
                        started.local_port().expect("remote relay listens on TCP")
                    }
                    TunnelKind::Local | TunnelKind::Dynamic => port,
                });
                relay = Some(started);
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

    let outcome = monitor_session(
        &spec.name,
        tx,
        cancel,
        &mut child,
        wait_for_relay(&mut relay),
        &stderr_tail,
    )
    .await;
    cleanup_after_exit(spec, relay, tx);
    outcome
}

async fn wait_for_relay(relay: &mut Option<Relay>) -> io::Result<()> {
    match relay {
        Some(relay) => relay.wait().await,
        None => pending().await,
    }
}

async fn monitor_session(
    name: &str,
    tx: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
    child: &mut Child,
    relay_exit: impl Future<Output = io::Result<()>>,
    stderr_tail: &Arc<Mutex<Vec<String>>>,
) -> RunOutcome {
    let started = Instant::now();
    let mut connected = false;

    // ExitOnForwardFailure=yes means a child that survives the grace period
    // has authenticated and bound its forwardings.
    let grace = sleep(CONNECT_GRACE);
    tokio::pin!(grace, relay_exit);
    loop {
        tokio::select! {
            status = child.wait() => {
                return RunOutcome::Exited {
                    reason: exit_reason(status, stderr_tail),
                    connected,
                    uptime: started.elapsed(),
                };
            }
            _ = cancelled(cancel) => {
                let _ = child.kill().await;
                return RunOutcome::Cancelled;
            }
            result = &mut relay_exit => {
                // Stop ssh before reconnecting, so an alive transport cannot
                // mask a dead relay and leave the UI permanently connected.
                let _ = child.kill().await;
                let reason = match result {
                    Ok(()) => "metering relay stopped unexpectedly".to_string(),
                    Err(error) => format!("metering relay failed: {error}"),
                };
                return RunOutcome::Exited { reason, connected, uptime: started.elapsed() };
            }
            _ = &mut grace, if !connected => {
                connected = true;
                send_state(tx, name, TunnelState::Connected { since: started });
            }
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
        && let Some(path) = spec.local_unix_socket()
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn assert_relay_failure_stops_session(after_connected: bool) {
        // A real long-lived child makes sure we actually kill/reap the ssh
        // stand-in, rather than just changing the displayed tunnel state.
        let mut child = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (tx, mut events) = mpsc::unbounded_channel();
        let (_cancel_tx, mut cancel) = watch::channel(false);
        let (fail_tx, fail_rx) = tokio::sync::oneshot::channel();
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));

        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(4), async {
            tokio::join!(
                monitor_session(
                    "test",
                    &tx,
                    &mut cancel,
                    &mut child,
                    async {
                        fail_rx.await.unwrap();
                        Err(io::Error::other("injected accept failure"))
                    },
                    &stderr_tail,
                ),
                async {
                    if after_connected {
                        loop {
                            let event = events.recv().await.unwrap();
                            if matches!(event.kind, EventKind::State(TunnelState::Connected { .. }))
                            {
                                break;
                            }
                        }
                    }
                    fail_tx.send(()).unwrap();
                },
            )
        })
        .await
        .expect("relay failure must end the session promptly");

        match outcome {
            RunOutcome::Exited {
                reason, connected, ..
            } => {
                assert_eq!(reason, "metering relay failed: injected accept failure");
                assert_eq!(connected, after_connected);
            }
            RunOutcome::Cancelled => panic!("relay failure was reported as cancellation"),
        }
        assert!(
            child.try_wait().unwrap().is_some(),
            "the ssh child must be reaped"
        );
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                event.kind,
                EventKind::State(TunnelState::Connected { .. })
            ));
        }
    }

    #[tokio::test]
    async fn relay_failure_during_startup_stops_session() {
        assert_relay_failure_stops_session(false).await;
    }

    #[tokio::test]
    async fn relay_failure_after_connect_stops_session() {
        assert_relay_failure_stops_session(true).await;
    }

    #[tokio::test]
    async fn unmetered_session_still_observes_child_exit() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 7"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (tx, _events) = mpsc::unbounded_channel();
        let (_cancel_tx, mut cancel) = watch::channel(false);
        let mut relay = None;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            monitor_session(
                "test",
                &tx,
                &mut cancel,
                &mut child,
                wait_for_relay(&mut relay),
                &stderr_tail,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            RunOutcome::Exited {
                connected: false,
                ..
            }
        ));
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(7));
    }
}
