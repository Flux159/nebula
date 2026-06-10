//! Cross-platform local IPC behind one path-addressed API.
//!
//! macOS/Linux: unix domain sockets at the given path (unchanged behavior).
//! Windows: loopback TCP with the bound port persisted in a file AT the
//! given path — Rust std doesn't expose AF_UNIX on Windows, and the docker
//! ecosystem there speaks npipe/tcp anyway. Same connect/listen call sites
//! everywhere; "is someone serving this path" probes work on both (a stale
//! port file simply refuses the connection).
//!
//! Security note (Windows): loopback TCP is reachable by other local users,
//! unlike unix sockets with file permissions. Acceptable for the dev
//! preview; the hardening path is named pipes with ACLs (tracked in
//! tasks/features.md Windows notes).

use std::io;
use std::path::Path;

#[cfg(unix)]
pub use unix_impl::*;
#[cfg(windows)]
pub use windows_impl::*;

#[cfg(unix)]
mod unix_impl {
    use super::*;
    pub type IpcStream = std::os::unix::net::UnixStream;
    pub type IpcListener = std::os::unix::net::UnixListener;

    /// Bind a fresh listener at `path` (replacing any stale socket file).
    pub fn listen(path: &Path) -> io::Result<IpcListener> {
        let _ = std::fs::remove_file(path);
        IpcListener::bind(path)
    }

    pub fn connect(path: &Path) -> io::Result<IpcStream> {
        IpcStream::connect(path)
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    pub type IpcStream = TcpStream;

    pub struct IpcListener {
        inner: TcpListener,
    }

    impl IpcListener {
        pub fn incoming(&self) -> impl Iterator<Item = io::Result<IpcStream>> + '_ {
            self.inner.incoming()
        }

        pub fn accept(&self) -> io::Result<(IpcStream, std::net::SocketAddr)> {
            self.inner.accept()
        }
    }

    /// Bind 127.0.0.1:<ephemeral> and persist the port in the file at `path`.
    pub fn listen(path: &Path) -> io::Result<IpcListener> {
        let inner = TcpListener::bind(("127.0.0.1", 0))?;
        let port = inner.local_addr()?.port();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(port.to_string().as_bytes())?;
        Ok(IpcListener { inner })
    }

    /// Read the port file at `path` and connect to it on loopback.
    pub fn connect(path: &Path) -> io::Result<IpcStream> {
        let mut s = String::new();
        std::fs::File::open(path)?.read_to_string(&mut s)?;
        let port: u16 = s
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad ipc port file"))?;
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}
