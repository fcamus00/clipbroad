//! File transfers: what this node serves ([`Offer`]) and how it fetches files from a peer.
//!
//! Each transfer is one bidirectional QUIC stream on the connection the peer already has with
//! this node: the receiver sends a [`FileRequest`], the sender answers with a [`FileResponse`]
//! and, if it serves the file, its raw bytes. See "File transport" in `docs/ARCHITECTURE.md`.
//!
//! # Trust
//!
//! - A node only serves the files of its latest broadcast clip, read from their original paths.
//!   Any other hash gets [`FileResponse::NotOffered`], and so does a file whose size changed
//!   since it was announced.
//! - The receiver checks the announced sizes against its limits before asking for anything,
//!   stops reading as soon as a peer sends more than it announced, and only exposes a file once
//!   its size and hash match (see [`FileCache`]).

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use clipbroad_config::Config;
use clipbroad_core::{
    ContentHash, FileEntry, FileList,
    protocol::{self, FileRequest, FileResponse, Message},
    validate_file_name,
};
use iroh::{
    EndpointId,
    endpoint::{
        ClosedStream, Connection, ConnectionError, ReadError, RecvStream, SendStream, WriteError,
    },
};
use tokio::{
    io::{AsyncReadExt as _, BufReader},
    sync::watch,
    time,
};
use tracing::{debug, warn};

use crate::{
    cache::{FileCache, StoreError},
    session::{FrameReader, READ_CHUNK, ReadFrameError},
};

/// Longest a peer may take to send its request after opening a stream.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Most bytes read for a request. A `FileRequest` frame takes about 40.
const MAX_REQUEST_LEN: usize = 1024;
/// Longest a transfer may go without receiving anything before it is given up.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a fetch waits for the peer to be connected.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Tries per file. A lost connection is retried on the next one; any other failure is final.
const ATTEMPTS: u32 = 3;
/// Read buffer when sending a file.
const SEND_BUFFER: usize = 256 * 1024;

/// Local files to announce in a clip and serve to the peers that ask for them.
///
/// Build it with [`Offer::read`], put [`Offer::list`] in a clip with
/// [`ClipContent::Files`](clipbroad_core::ClipContent::Files), and send that clip with
/// [`Node::broadcast_files`](crate::Node::broadcast_files).
#[derive(Debug, Clone)]
pub struct Offer {
    list: FileList,
    paths: HashMap<ContentHash, OfferedFile>,
}

#[derive(Debug, Clone)]
struct OfferedFile {
    path: PathBuf,
    size: u64,
}

impl Offer {
    /// Hashes the local files at `paths` and prepares to serve them. Blocks on the file system:
    /// it reads every file once (BLAKE3 hashes about 100 MB in 17 ms).
    ///
    /// Skips, with a warning: directories and anything else that is not a regular file (folders
    /// are not supported yet), files larger than `config.file_max_bytes`, names other machines
    /// would reject (see [`validate_file_name`]) and a second file with the same name. Returns
    /// `None` if nothing is left, or if the files add up to more than `file_cache_max_bytes`.
    ///
    /// # Errors
    ///
    /// If a file cannot be read.
    pub fn read(paths: &[PathBuf], config: &Config) -> io::Result<Option<Self>> {
        let max_file_bytes = config.file_max_bytes.get();
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut offered = HashMap::new();
        for path in paths {
            let meta = fs::metadata(path)?;
            if !meta.is_file() {
                warn!(path = %path.display(), "not a regular file (folders are not supported yet), not synced");
                continue;
            }
            if meta.len() > max_file_bytes {
                warn!(path = %path.display(), size = meta.len(), max_file_bytes, "file is larger than file_max_bytes, not synced");
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                warn!(path = %path.display(), "file name is not valid UTF-8, not synced");
                continue;
            };
            if let Err(reason) = validate_file_name(name) {
                warn!(path = %path.display(), %reason, "file name is not portable, not synced");
                continue;
            }
            if entries.iter().any(|e| e.name == name) {
                warn!(path = %path.display(), "another copied file has the same name, not synced");
                continue;
            }
            let (size, hash) = hash_file(path)?;
            if size > max_file_bytes {
                warn!(path = %path.display(), size, max_file_bytes, "file grew past file_max_bytes, not synced");
                continue;
            }
            entries.push(FileEntry {
                name: name.to_owned(),
                size,
                hash,
            });
            offered.insert(
                hash,
                OfferedFile {
                    path: path.clone(),
                    size,
                },
            );
        }
        let Ok(list) = FileList::new(entries) else {
            // Every entry was checked above, so the list is only rejected for being empty.
            return Ok(None);
        };
        let max_bytes = config.file_cache_max_bytes.get();
        if list.total_size() > max_bytes {
            warn!(
                total = list.total_size(),
                max_bytes, "copied files add up to more than file_cache_max_bytes, not synced"
            );
            return Ok(None);
        }
        Ok(Some(Self {
            list,
            paths: offered,
        }))
    }

    /// The files to announce.
    #[must_use]
    pub fn list(&self) -> &FileList {
        &self.list
    }
}

/// BLAKE3 and size of the file at `path`, from one read of its bytes.
fn hash_file(path: &Path) -> io::Result<(u64, ContentHash)> {
    let mut hasher = blake3::Hasher::new();
    let size = io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok((size, ContentHash::from_bytes(*hasher.finalize().as_bytes())))
}

/// Why a file could not be fetched.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FetchError {
    #[error("a file of {size} bytes is over the file_max_bytes limit ({max})")]
    FileTooLarge { size: u64, max: u64 },
    #[error("the files add up to {size} bytes, over the file_cache_max_bytes limit ({max})")]
    ClipTooLarge { size: u64, max: u64 },
    #[error("not a configured peer")]
    UnknownPeer,
    #[error("peer is not connected")]
    NotConnected,
    #[error("peer no longer offers the file (a newer copy, or the file changed)")]
    NotOffered,
    #[error("peer sent nothing for {STALL_TIMEOUT:?}")]
    Stalled,
    #[error("peer broke the file protocol: {0}")]
    Protocol(&'static str),
    #[error("peer offers {offered} bytes for a file announced with {announced}")]
    SizeMismatch { offered: u64, announced: u64 },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Closed(#[from] ClosedStream),
    #[error(transparent)]
    Frame(#[from] protocol::FrameError),
    #[error("file cache error")]
    Cache(#[from] io::Error),
}

impl FetchError {
    /// Whether the connection went away mid-transfer, so a new connection may succeed.
    fn connection_lost(&self) -> bool {
        matches!(
            self,
            Self::Connection(_)
                | Self::Read(ReadError::ConnectionLost(_))
                | Self::Write(WriteError::ConnectionLost(_))
        )
    }
}

impl From<ReadFrameError> for FetchError {
    fn from(error: ReadFrameError) -> Self {
        match error {
            ReadFrameError::Read(e) => Self::Read(e),
            ReadFrameError::Frame(e) => Self::Frame(e),
            ReadFrameError::Truncated => Self::Protocol("stream ended inside the response"),
        }
    }
}

/// Fetches the files of `files` into `cache`, one after the other, skipping those already there.
/// Returns their cache paths in list order. Then cleans the cache, keeping these files.
///
/// Dropping the future cancels the transfer: the stream is closed and the temporary file deleted.
pub(crate) async fn fetch(
    mut conn: watch::Receiver<Vec<Connection>>,
    files: &FileList,
    cache: &FileCache,
) -> Result<Vec<PathBuf>, FetchError> {
    for entry in files.entries() {
        if entry.size > cache.max_file_bytes() {
            return Err(FetchError::FileTooLarge {
                size: entry.size,
                max: cache.max_file_bytes(),
            });
        }
    }
    if files.total_size() > cache.max_bytes() {
        return Err(FetchError::ClipTooLarge {
            size: files.total_size(),
            max: cache.max_bytes(),
        });
    }

    let mut paths = Vec::with_capacity(files.entries().len());
    for entry in files.entries() {
        match blocking(cache, entry, FileCache::contains).await {
            Ok(true) => {
                debug!(hash = %entry.hash, "file already in the cache");
                paths.push(cache.path_of(entry));
                continue;
            }
            Ok(false) => {}
            // Downloading it again replaces the file, or fails with a clearer error.
            Err(error) => debug!(hash = %entry.hash, %error, "cannot check the cached file"),
        }
        paths.push(fetch_with_retries(&mut conn, entry, cache).await?);
    }

    let (cache, keep) = (cache.clone(), files.clone());
    match tokio::task::spawn_blocking(move || cache.clean(&keep)).await {
        Ok(Ok(cleaned)) if cleaned.files > 0 => {
            debug!(
                files = cleaned.files,
                bytes = cleaned.bytes,
                "cleaned the file cache"
            );
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(%error, "could not clean the file cache"),
        Err(error) => warn!(%error, "file cache cleaning stopped"),
    }
    Ok(paths)
}

/// Runs a blocking cache operation on `entry` off the async thread.
async fn blocking<T: Send + 'static>(
    cache: &FileCache,
    entry: &FileEntry,
    op: fn(&FileCache, &FileEntry) -> io::Result<T>,
) -> io::Result<T> {
    let (cache, entry) = (cache.clone(), entry.clone());
    tokio::task::spawn_blocking(move || op(&cache, &entry))
        .await
        .map_err(io::Error::other)?
}

async fn fetch_with_retries(
    conn: &mut watch::Receiver<Vec<Connection>>,
    entry: &FileEntry,
    cache: &FileCache,
) -> Result<PathBuf, FetchError> {
    let mut attempt = 1;
    loop {
        let open = |conn: &Connection| conn.close_reason().is_none();
        // The borrowed list is a read lock on the peer's connections: release it before the
        // transfer, or the peer task would block replacing a connection meanwhile.
        let current =
            match time::timeout(CONNECT_TIMEOUT, conn.wait_for(|live| live.iter().any(open))).await
            {
                Ok(Ok(live)) => live.iter().rev().find(|conn| open(conn)).cloned(),
                _ => None,
            }
            .ok_or(FetchError::NotConnected)?;
        match transfer(&current, entry, cache).await {
            Err(error) if error.connection_lost() && attempt < ATTEMPTS => {
                debug!(%error, attempt, "file transfer lost its connection, trying again");
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Asks for one file on a new stream and stores it in the cache once verified.
async fn transfer(
    conn: &Connection,
    entry: &FileEntry,
    cache: &FileCache,
) -> Result<PathBuf, FetchError> {
    let (mut send, recv) = conn.open_bi().await?;
    let request = protocol::encode(&Message::FileRequest(FileRequest { hash: entry.hash }))?;
    send.write_all(&request).await?;
    send.finish()?;

    let mut reader = FrameReader::new(recv);
    let response = time::timeout(STALL_TIMEOUT, reader.next())
        .await
        .map_err(|_| FetchError::Stalled)??;
    let offered = match response {
        Some(Message::FileResponse(FileResponse::Ok { size })) => size,
        Some(Message::FileResponse(FileResponse::NotOffered)) => {
            return Err(FetchError::NotOffered);
        }
        Some(_) => return Err(FetchError::Protocol("unexpected message")),
        None => return Err(FetchError::Protocol("stream ended before the response")),
    };
    if offered != entry.size {
        return Err(FetchError::SizeMismatch {
            offered,
            announced: entry.size,
        });
    }

    let (mut recv, first_bytes) = reader.into_parts();
    let mut incoming = blocking(cache, entry, FileCache::begin).await?;
    incoming.write(&first_bytes).await?;
    loop {
        let chunk = time::timeout(STALL_TIMEOUT, recv.read_chunk(READ_CHUNK))
            .await
            .map_err(|_| FetchError::Stalled)??;
        match chunk {
            Some(chunk) => incoming.write(&chunk).await?,
            None => break,
        }
    }
    Ok(incoming.finish().await?)
}

/// Answers one file request from `peer`: the file if `offer` has it, `NotOffered` otherwise.
pub(crate) async fn serve(
    send: SendStream,
    recv: RecvStream,
    offer: Option<Arc<Offer>>,
    peer: EndpointId,
) {
    let peer = peer.fmt_short();
    match serve_inner(send, recv, offer.as_deref()).await {
        Ok(()) => {}
        Err(ServeError::Request(reason)) => warn!(%peer, reason, "bad file request"),
        Err(error) => debug!(%peer, %error, "file transfer ended early"),
    }
}

#[derive(Debug, thiserror::Error)]
enum ServeError {
    #[error("{0}")]
    Request(&'static str),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Closed(#[from] ClosedStream),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] protocol::FrameError),
}

async fn serve_inner(
    mut send: SendStream,
    mut recv: RecvStream,
    offer: Option<&Offer>,
) -> Result<(), ServeError> {
    // The requester finishes its side after the request, so it is all there is to read.
    let request = time::timeout(REQUEST_TIMEOUT, recv.read_to_end(MAX_REQUEST_LEN))
        .await
        .map_err(|_| ServeError::Request("no request in time"))?
        .map_err(|_| ServeError::Request("request too long or stream broken"))?;
    let hash = match protocol::decode(&request) {
        Ok(Some((Message::FileRequest(FileRequest { hash }), used))) if used == request.len() => {
            hash
        }
        _ => return Err(ServeError::Request("not a single file request")),
    };
    let opened = match offer.and_then(|offer| offer.paths.get(&hash)) {
        Some(file) => open_unchanged(file).await,
        None => None,
    };
    let Some((file, size)) = opened else {
        send.write_all(&encode(FileResponse::NotOffered)?).await?;
        send.finish()?;
        return Ok(());
    };
    send.write_all(&encode(FileResponse::Ok { size })?).await?;
    let mut body = BufReader::with_capacity(SEND_BUFFER, file.take(size));
    tokio::io::copy_buf(&mut body, &mut send).await?;
    send.finish()?;
    Ok(())
}

/// Opens an offered file if it still has the size it was announced with.
async fn open_unchanged(file: &OfferedFile) -> Option<(tokio::fs::File, u64)> {
    let opened = tokio::fs::File::open(&file.path).await;
    let handle = match opened {
        Ok(handle) => handle,
        Err(error) => {
            warn!(path = %file.path.display(), %error, "cannot read an offered file");
            return None;
        }
    };
    let size = handle.metadata().await.ok()?.len();
    if size != file.size {
        debug!(path = %file.path.display(), "offered file changed size since it was copied");
        return None;
    }
    Some((handle, size))
}

fn encode(response: FileResponse) -> Result<Vec<u8>, protocol::FrameError> {
    protocol::encode(&Message::FileResponse(response))
}
