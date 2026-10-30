//! Windows: a named pipe whose name comes from the path of `control.sock` beside the identity
//! file. No file is created there.
//!
//! A pipe vanishes with the process that created it, so nothing is left over after a crash. The
//! pipe refuses remote clients, and its default security lets only this user, administrators
//! and the system open it for writing.

use std::io;
use std::path::Path;
use std::time::Duration;

use clipbroad_config::Paths;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::{self, Instant};

use super::socket_path;

/// Win32 error when every instance of the pipe is busy with another client.
const ERROR_PIPE_BUSY: i32 = 231;
/// How long a client waits for a free pipe instance.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// The daemon's end of a connection.
pub type ServerStream = NamedPipeServer;

/// Where the daemon listens: the pipe instance the next client connects to.
#[derive(Debug)]
pub struct Listener {
    name: String,
    next: NamedPipeServer,
}

impl Listener {
    /// Creates the pipe for `paths`.
    ///
    /// # Errors
    ///
    /// If another process created it already (another daemon with the same config), or it
    /// cannot be created.
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        let name = pipe_name(&socket_path(paths))?;
        let next = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)?;
        Ok(Self { name, next })
    }

    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        self.next.connect().await?;
        let next = ServerOptions::new()
            .reject_remote_clients(true)
            .create(&self.name)?;
        Ok(std::mem::replace(&mut self.next, next))
    }
}

/// Connects to the daemon for `paths`, or returns `None` if none is listening.
///
/// # Errors
///
/// If the pipe exists but cannot be opened, or stays busy.
pub async fn connect(paths: &Paths) -> io::Result<Option<NamedPipeClient>> {
    let name = pipe_name(&socket_path(paths))?;
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => return Ok(Some(client)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
            {
                time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// `\\.\pipe\clipbroad\` and the absolute path in lowercase, with `/` for `\`, which pipe names
/// cannot hold. Windows paths ignore case, so a `--config` spelled differently names the same
/// pipe.
fn pipe_name(socket: &Path) -> io::Result<String> {
    let path = std::path::absolute(socket)?;
    let path = path.to_string_lossy().to_lowercase().replace('\\', "/");
    Ok(format!(r"\\.\pipe\clipbroad\{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_comes_from_the_path() {
        assert_eq!(
            pipe_name(Path::new(
                r"C:\Users\Me\AppData\Local\clipbroad\control.sock"
            ))
            .expect("absolute path"),
            r"\\.\pipe\clipbroad\c:/users/me/appdata/local/clipbroad/control.sock"
        );
    }
}
