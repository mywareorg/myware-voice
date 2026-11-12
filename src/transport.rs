//! The wire between the compositor's keybind and the daemon.
//!
//! Unix uses a socket in `$XDG_RUNTIME_DIR`, whose directory permissions already
//! say "this user only". Windows has no such thing, so it uses loopback TCP with
//! an ephemeral port and a per-run token written to the state directory: without
//! the token the port is merely open, and every request must carry it.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

/// One connection. Read and write, nothing else.
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub fn endpoint_path() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!("XDG_RUNTIME_DIR is not set; no place for the socket")
            })?;
        Ok(base.join("hippo-voice/daemon.sock"))
    }
    #[cfg(windows)]
    {
        Ok(crate::log::dir()?.join("endpoint"))
    }
}

#[cfg(unix)]
pub struct Listener(std::os::unix::net::UnixListener);

#[cfg(unix)]
impl Listener {
    pub fn bind() -> Result<Self> {
        let path = endpoint_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::os::unix::net::UnixListener::bind(&path) {
            Ok(listener) => Ok(Self(listener)),
            Err(_) if std::os::unix::net::UnixStream::connect(&path).is_ok() => {
                anyhow::bail!("another daemon is already listening on {}", path.display())
            }
            Err(_) => {
                // A stale socket from a crash: nothing is listening, replace it.
                let _ = std::fs::remove_file(&path);
                Ok(Self(std::os::unix::net::UnixListener::bind(&path)?))
            }
        }
    }

    pub fn accept(&self) -> io::Result<Box<dyn Stream>> {
        let (stream, _) = self.0.accept()?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        Ok(Box::new(stream))
    }

    pub fn token(&self) -> Option<String> {
        None
    }
}

#[cfg(unix)]
pub fn connect() -> Result<Box<dyn Stream>> {
    let path = endpoint_path()?;
    let stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("connect {}", path.display()))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    Ok(Box::new(stream))
}

#[cfg(unix)]
pub fn token() -> Option<String> {
    None
}

#[cfg(windows)]
pub struct Listener {
    tcp: std::net::TcpListener,
    token: String,
}

#[cfg(windows)]
impl Listener {
    pub fn bind() -> Result<Self> {
        use std::io::Write as _;

        let tcp =
            std::net::TcpListener::bind(("127.0.0.1", 0)).context("bind the loopback listener")?;
        let port = tcp.local_addr()?.port();
        let token = random_token();

        // The port alone is not authority to toggle the microphone, so the
        // token lives in a file only this user can read.
        let path = endpoint_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::File::create(&path)?;
        writeln!(file, "{port}")?;
        writeln!(file, "{token}")?;

        Ok(Self { tcp, token })
    }

    pub fn accept(&self) -> io::Result<Box<dyn Stream>> {
        let (stream, _) = self.tcp.accept()?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        Ok(Box::new(stream))
    }

    pub fn token(&self) -> Option<String> {
        Some(self.token.clone())
    }
}

/// 32 hex characters from the OS random source.
#[cfg(windows)]
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:016x}{:016x}", hasher.finish(), std::process::id())
}

#[cfg(windows)]
pub fn connect() -> Result<Box<dyn Stream>> {
    let path = endpoint_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}; is the daemon running?", path.display()))?;
    let mut lines = text.lines();
    let port: u16 = lines
        .next()
        .and_then(|line| line.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("{} has no port", path.display()))?;
    let stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    Ok(Box::new(stream))
}

#[cfg(windows)]
pub fn token() -> Option<String> {
    let text = std::fs::read_to_string(endpoint_path().ok()?).ok()?;
    text.lines().nth(1).map(|line| line.trim().to_string())
}
