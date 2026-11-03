//! In-process byte-counting relay.
//!
//! Metered tunnels put this relay on the local leg of the forwarding chain:
//! for `-L`/`-D` it owns the user-facing listener and forwards into ssh's
//! loopback port; for `-R` it sits between ssh and the real local target.
//! Either way every forwarded byte passes through `copy_counted`.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Debug, Default)]
pub struct Counters {
    pub client_to_upstream: AtomicU64,
    pub upstream_to_client: AtomicU64,
    pub total_connections: AtomicU64,
    pub active_connections: AtomicU64,
}

pub type SharedCounters = Arc<Counters>;

pub enum ListenSpec {
    Tcp(String),
    Unix(PathBuf),
}

#[derive(Clone)]
pub enum UpstreamSpec {
    Tcp(String),
    Unix(PathBuf),
}

pub struct Relay {
    task: JoinHandle<io::Result<()>>,
    socket_path: Option<PathBuf>,
    local_port: Option<u16>,
}

impl Relay {
    pub async fn start(
        listen: ListenSpec,
        upstream: UpstreamSpec,
        counters: SharedCounters,
    ) -> io::Result<Self> {
        match listen {
            ListenSpec::Tcp(addr) => {
                let listener = TcpListener::bind(&addr).await?;
                let local_port = Some(listener.local_addr()?.port());
                let task = tokio::spawn(async move {
                    accept_connections(
                        || async {
                            let (stream, _) = listener.accept().await?;
                            let _ = stream.set_nodelay(true);
                            Ok(stream)
                        },
                        upstream,
                        counters,
                    )
                    .await
                });
                Ok(Self {
                    task,
                    socket_path: None,
                    local_port,
                })
            }
            ListenSpec::Unix(path) => {
                remove_stale_socket(&path)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let listener = UnixListener::bind(&path)?;
                let task = tokio::spawn(async move {
                    accept_connections(
                        || async { listener.accept().await.map(|(stream, _)| stream) },
                        upstream,
                        counters,
                    )
                    .await
                });
                Ok(Self {
                    task,
                    socket_path: Some(path),
                    local_port: None,
                })
            }
        }
    }

    pub fn local_port(&self) -> Option<u16> {
        self.local_port
    }

    /// The supervisor must watch the relay as well as ssh: an authenticated
    /// ssh session can stay alive after its local forwarding destination dies.
    pub async fn wait(&mut self) -> io::Result<()> {
        (&mut self.task)
            .await
            .map_err(|error| io::Error::other(format!("relay task failed: {error}")))?
    }
}

async fn accept_connections<S, F, Fut>(
    mut accept: F,
    upstream: UpstreamSpec,
    counters: SharedCounters,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    // Session-owned tasks are aborted when the relay ends. Detached copies
    // otherwise keep old sockets alive across reconnects.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = accept() => match result {
                Ok(stream) => {
                    connections.spawn(handle_conn(stream, upstream.clone(), counters.clone()));
                }
                Err(error) if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::TimedOut
                ) => {
                    // A peer can disappear while queued for accept. Keep the
                    // listener alive and avoid spinning if the error repeats.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error),
            },
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(path) = &self.socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Remove a leftover socket file, refusing to touch anything that is not a
/// unix socket so a mistyped path can never delete real data.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a socket; refusing to remove it",
                path.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub async fn pick_free_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    Ok(listener.local_addr()?.port())
}

async fn handle_conn<S>(client: S, upstream: UpstreamSpec, counters: SharedCounters)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    counters.total_connections.fetch_add(1, Ordering::Relaxed);
    counters.active_connections.fetch_add(1, Ordering::Relaxed);
    let _active = ActiveConnection(counters.clone());
    match upstream {
        UpstreamSpec::Tcp(addr) => {
            if let Ok(up) = TcpStream::connect(&addr).await {
                let _ = up.set_nodelay(true);
                pipe(client, up, &counters).await;
            }
        }
        UpstreamSpec::Unix(path) => {
            if let Ok(up) = UnixStream::connect(&path).await {
                pipe(client, up, &counters).await;
            }
        }
    }
}

struct ActiveConnection(SharedCounters);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn pipe<A, B>(a: A, b: B, counters: &Counters)
where
    A: AsyncRead + AsyncWrite + Send + Unpin,
    B: AsyncRead + AsyncWrite + Send + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    // An I/O error makes the whole connection unusable. Cancel its other
    // direction instead of waiting indefinitely and retaining both sockets.
    // Successful EOF still waits for the half-closed peer's response.
    let _ = tokio::try_join!(
        copy_counted(&mut a_read, &mut b_write, &counters.client_to_upstream),
        copy_counted(&mut b_read, &mut a_write, &counters.upstream_to_client),
    );
}

async fn copy_counted<R, W>(reader: &mut R, writer: &mut W, counter: &AtomicU64) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            // Propagate EOF as a half-close so protocols that shut down one
            // direction first keep working.
            return writer.shutdown().await;
        }
        writer.write_all(&buffer[..n]).await?;
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ResetReader;

    impl AsyncRead for ResetReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()))
        }
    }

    #[tokio::test]
    async fn connection_error_closes_both_directions() {
        let client = tokio::io::join(ResetReader, tokio::io::sink());
        let (upstream, _idle_peer) = tokio::io::duplex(64);
        let counters = Counters::default();

        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            pipe(client, upstream, &counters),
        )
        .await
        .expect("a reset must close the connection even when the other direction is idle");
    }

    #[tokio::test]
    async fn retries_transient_accept_errors_and_still_relays_traffic() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut request = [0; 5];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"hello");
            stream.write_all(b"world").await.unwrap();
        });

        let (mut client, accepted) = tokio::io::duplex(64);
        let mut accepts: std::collections::VecDeque<_> = [
            io::ErrorKind::Interrupted,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
        ]
        .into_iter()
        .map(|kind| Err(io::Error::from(kind)))
        .chain(std::iter::once(Ok(accepted)))
        .collect();
        let counters = Arc::new(Counters::default());
        let task = tokio::spawn(accept_connections(
            move || {
                let result = accepts.pop_front();
                async move {
                    match result {
                        Some(result) => result,
                        None => std::future::pending().await,
                    }
                }
            },
            UpstreamSpec::Tcp(upstream_addr.to_string()),
            counters.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            client.write_all(b"hello").await.unwrap();
            let mut response = [0; 5];
            client.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"world");
            server.await.unwrap();
        })
        .await
        .expect("accept errors must not stop the listener");
        assert_eq!(counters.total_connections.load(Ordering::Relaxed), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn reports_fatal_accept_errors() {
        let mut relay = Relay {
            task: tokio::spawn(accept_connections(
                || {
                    std::future::ready(Err::<tokio::io::DuplexStream, _>(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected accept failure",
                    )))
                },
                UpstreamSpec::Tcp("127.0.0.1:1".to_string()),
                Arc::new(Counters::default()),
            )),
            socket_path: None,
            local_port: None,
        };
        let error = relay.wait().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "injected accept failure");
    }

    #[tokio::test]
    async fn preserves_half_close_until_the_response_arrives() {
        let (mut client, a) = tokio::io::duplex(64);
        let (b, mut upstream) = tokio::io::duplex(64);
        let counters = Arc::new(Counters::default());
        let task_counters = counters.clone();
        let task = tokio::spawn(async move { pipe(a, b, &task_counters).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            client.write_all(b"request").await.unwrap();
            client.shutdown().await.unwrap();
            let mut request = Vec::new();
            upstream.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"request");
            upstream.write_all(b"response").await.unwrap();
            upstream.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, b"response");
            task.await.unwrap();
        })
        .await
        .expect("EOF must allow a response through the remaining direction");
        assert_eq!(counters.client_to_upstream.load(Ordering::Relaxed), 7);
        assert_eq!(counters.upstream_to_client.load(Ordering::Relaxed), 8);
    }

    #[tokio::test]
    async fn stopping_relay_closes_active_connections() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let counters = Arc::new(Counters::default());
        let relay = Relay::start(
            ListenSpec::Tcp("127.0.0.1:0".to_string()),
            UpstreamSpec::Tcp(upstream_addr.to_string()),
            counters.clone(),
        )
        .await
        .unwrap();
        let port = relay.local_port().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            let (mut server, _) = upstream.accept().await.unwrap();
            assert_eq!(counters.active_connections.load(Ordering::Relaxed), 1);
            drop(relay);
            let mut byte = [0; 1];
            assert_eq!(client.read(&mut byte).await.unwrap(), 0);
            assert_eq!(server.read(&mut byte).await.unwrap(), 0);
            while counters.active_connections.load(Ordering::Relaxed) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("relay shutdown must release its connections and counters");
    }

    #[tokio::test]
    async fn relays_and_counts_tcp_traffic() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
            stream.write_all(b"world!!").await.unwrap();
        });

        let counters: SharedCounters = Arc::new(Counters::default());
        let relay = Relay::start(
            ListenSpec::Tcp("127.0.0.1:0".to_string()),
            UpstreamSpec::Tcp(upstream_addr.to_string()),
            counters.clone(),
        )
        .await
        .unwrap();
        let port = relay.local_port().unwrap();

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 7];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world!!");

        assert_eq!(counters.client_to_upstream.load(Ordering::Relaxed), 5);
        assert_eq!(counters.upstream_to_client.load(Ordering::Relaxed), 7);
        assert_eq!(counters.total_connections.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn unix_listener_cleans_socket_on_drop() {
        let dir = std::env::temp_dir().join(format!("rumole-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("relay.sock");

        let counters: SharedCounters = Arc::new(Counters::default());
        let relay = Relay::start(
            ListenSpec::Unix(sock.clone()),
            UpstreamSpec::Tcp("127.0.0.1:1".to_string()),
            counters,
        )
        .await
        .unwrap();
        assert!(sock.exists());
        drop(relay);
        assert!(!sock.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_remove_non_socket() {
        let dir = std::env::temp_dir().join(format!("rumole-test-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-socket");
        std::fs::write(&file, "data").unwrap();
        assert!(remove_stale_socket(&file).is_err());
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
