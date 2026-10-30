//! Linux and macOS: a Unix socket, `control.sock` beside the identity file.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use clipbroad_config::Paths;
use tokio::net::{UnixListener, UnixStream};

use super::socket_path;

/// The daemon's end of a connection.
pub type ServerStream = UnixStream;

/// Where the daemon listens. Dropping it removes the socket file.
#[derive(Debug)]
pub struct Listener {
    socket: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Listens on the socket for `paths`. A socket file that nobody answers on is left over from
    /// a daemon that did not stop cleanly, and is replaced.
    ///
    /// # Errors
    ///
    /// If another daemon answers on it, or the socket cannot be created.
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        let path = socket_path(paths);
        let socket = match UnixListener::bind(&path) {
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    return Err(error);
                }
                fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            result => result?,
        };
        // The directory is private already; this also keeps others out of a shared one.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { socket, path })
    }

    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        Ok(self.socket.accept().await?.0)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Connects to the daemon for `paths`, or returns `None` if none is listening.
///
/// # Errors
///
/// If the socket exists but cannot be used, e.g. for lack of permissions.
pub async fn connect(paths: &Paths) -> io::Result<Option<UnixStream>> {
    match UnixStream::connect(socket_path(paths)).await {
        Ok(stream) => Ok(Some(stream)),
        // No socket, or one left over from a daemon that did not stop cleanly.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
