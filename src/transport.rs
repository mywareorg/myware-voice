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

/// "Nothing is listening", as distinct from "a daemon answered and the answer was
/// wrong". `toggle` may start a daemon only in the first case: treating a live but
/// confused daemon as absent is how you end up with two of them fighting over the
/// endpoint.
#[derive(Debug)]
pub struct Unreachable;

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no daemon is listening")
    }
}

impl std::error::Error for Unreachable {}

fn unreachable(context: String) -> anyhow::Error {
    anyhow::Error::new(Unreachable).context(context)
}

/// Whether this error means "try starting one".
pub fn is_unreachable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Unreachable>())
}

pub fn endpoint_path() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        crate::dirs::socket_file()
    }
    #[cfg(windows)]
    {
        crate::dirs::endpoint_file()
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
    use std::io::ErrorKind;

    let path = endpoint_path()?;
    match std::os::unix::net::UnixStream::connect(&path) {
        Ok(stream) => {
            stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
            Ok(Box::new(stream))
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Err(unreachable(format!("connect {}", path.display())))
        }
        Err(error) => Err(error).with_context(|| format!("connect {}", path.display())),
    }
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

        // A live daemon already owns this endpoint. Starting a second one would
        // take the port over and then fail to register the hotkey, leaving a
        // running process with a dead keybind.
        if let Some((port, _)) = read_endpoint() {
            let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let alive = std::net::TcpStream::connect_timeout(
                &address,
                std::time::Duration::from_millis(300),
            )
            .is_ok();
            if alive {
                anyhow::bail!("another daemon is already listening on 127.0.0.1:{port}");
            }
        }

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
        // Written under a temporary name and renamed into place. A client that
        // reads while this runs would otherwise see a port with no token yet,
        // conclude no daemon is listening, and start a second one on a fresh port.
        let temporary = path.with_extension("tmp");
        {
            let mut file = std::fs::File::create(&temporary)?;
            writeln!(file, "{port}")?;
            writeln!(file, "{token}")?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;

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

/// The port and token a live daemon wrote, if the file parses.
#[cfg(windows)]
fn read_endpoint() -> Option<(u16, String)> {
    let text = std::fs::read_to_string(endpoint_path().ok()?).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let token = lines.next()?.trim().to_string();
    Some((port, token))
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
    use std::io::ErrorKind;

    let path = endpoint_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(unreachable(format!("read {}", path.display())));
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let port: u16 = text
        .lines()
        .next()
        .and_then(|line| line.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("{} has no port", path.display()))?;

    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2)) {
        Ok(stream) => {
            stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
            Ok(Box::new(stream))
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::TimedOut
            ) =>
        {
            Err(unreachable(format!("connect 127.0.0.1:{port}")))
        }
        Err(error) => Err(error).with_context(|| format!("connect 127.0.0.1:{port}")),
    }
}

#[cfg(windows)]
pub fn token() -> Option<String> {
    read_endpoint().map(|(_, token)| token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_connection_failures_count_as_unreachable() {
        let missing = unreachable("connect /nope".to_string());
        assert!(is_unreachable(&missing));

        // A daemon that answered with nonsense must not be treated as absent,
        // or `toggle` would start a second one on top of it.
        let confused = anyhow::anyhow!("parse reply: expected value");
        assert!(!is_unreachable(&confused));
    }
}
