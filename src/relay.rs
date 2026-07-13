//! In-process byte-counting relay.
//!
//! Metered tunnels put this relay on the local leg of the forwarding chain:
//! for `-L`/`-D` it owns the user-facing listener and forwards into ssh's
//! loopback port; for `-R` it sits between ssh and the real local target.
//! Either way every forwarded byte passes through `copy_counted`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::task::JoinHandle;

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
    task: JoinHandle<()>,
    socket_path: Option<PathBuf>,
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
                let task = tokio::spawn(async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            break;
                        };
                        let _ = stream.set_nodelay(true);
                        tokio::spawn(handle_conn(stream, upstream.clone(), counters.clone()));
                    }
                });
                Ok(Self { task, socket_path: None })
            }
            ListenSpec::Unix(path) => {
                remove_stale_socket(&path)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let listener = UnixListener::bind(&path)?;
                let task = tokio::spawn(async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            break;
                        };
                        tokio::spawn(handle_conn(stream, upstream.clone(), counters.clone()));
                    }
                });
                Ok(Self { task, socket_path: Some(path) })
            }
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
            format!("{} exists and is not a socket; refusing to remove it", path.display()),
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
    counters.active_connections.fetch_sub(1, Ordering::Relaxed);
}

async fn pipe<A, B>(a: A, b: B, counters: &Counters)
where
    A: AsyncRead + AsyncWrite + Send + Unpin,
    B: AsyncRead + AsyncWrite + Send + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let _ = tokio::join!(
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
            let _ = writer.shutdown().await;
            return Ok(());
        }
        writer.write_all(&buffer[..n]).await?;
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let port = pick_free_port().await.unwrap();
        let counters: SharedCounters = Arc::new(Counters::default());
        let _relay = Relay::start(
            ListenSpec::Tcp(format!("127.0.0.1:{port}")),
            UpstreamSpec::Tcp(upstream_addr.to_string()),
            counters.clone(),
        )
        .await
        .unwrap();

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
