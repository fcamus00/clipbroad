//! File transfers between two nodes, and against raw peers that serve or ask for things a real
//! node never would.

use std::{
    fs,
    num::{NonZeroU32, NonZeroU64},
    path::Path,
};

use clipbroad_core::{
    ContentHash, FileEntry,
    protocol::{FileRequest, FileResponse},
};

use super::*;

/// A 40 MiB transfer in a debug build on a slow CI runner can take a few seconds.
const BIG_TIMEOUT: Duration = Duration::from_secs(60);

/// Deterministic bytes that do not compress or repeat.
fn random_bytes(len: usize, seed: u8) -> Vec<u8> {
    let mut bytes = vec![0; len];
    blake3::Hasher::new()
        .update(&[seed])
        .finalize_xof()
        .fill(&mut bytes);
    bytes
}

fn write_files(dir: &Path, files: &[(&str, &[u8])]) -> Vec<PathBuf> {
    files
        .iter()
        .map(|(name, bytes)| {
            let path = dir.join(name);
            fs::write(&path, bytes).expect("write source file");
            path
        })
        .collect()
}

/// Limits of 1 MiB per file and 2 MiB per clip and cache.
fn small_limits() -> Config {
    Config {
        file_max_bytes: NonZeroU64::new(1 << 20).expect("non-zero"),
        file_cache_max_bytes: NonZeroU64::new(2 << 20).expect("non-zero"),
        file_cache_max_days: NonZeroU32::new(7).expect("non-zero"),
        ..Config::default()
    }
}

fn open_cache(dir: &tempfile::TempDir, config: &Config) -> FileCache {
    FileCache::open(dir.path().join("files"), config).expect("cache opens")
}

fn files_clip(origin: EndpointId, id: u64, files: FileList) -> Clip {
    Clip::new(
        ClipId(id),
        NodeId::from_bytes(*origin.as_bytes()),
        1_700_000_000_000,
        ClipContent::Files(files),
    )
}

fn entry(name: &str, bytes: &[u8]) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        size: bytes.len() as u64,
        hash: ContentHash::of(bytes),
    }
}

fn one_file(entry: &FileEntry) -> FileList {
    FileList::new(vec![entry.clone()]).expect("valid list")
}

/// Every file in the cache, temporary ones included.
fn cached_files(cache: &FileCache) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in fs::read_dir(cache.dir()).expect("read cache") {
        for file in fs::read_dir(dir.expect("entry").path()).expect("read hash dir") {
            files.push(file.expect("entry").path());
        }
    }
    files
}

fn response(response: FileResponse) -> Vec<u8> {
    protocol::encode(&Message::FileResponse(response)).expect("encode")
}

/// Waits for the node to ask for a file. Returns the stream to answer on and the hash asked for.
async fn accept_request(conn: &Connection) -> (SendStream, ContentHash) {
    let (send, mut recv) = time::timeout(TIMEOUT, conn.accept_bi())
        .await
        .expect("request in time")
        .expect("stream");
    let Message::FileRequest(FileRequest { hash }) = read(&mut recv).await else {
        panic!("a file request");
    };
    (send, hash)
}

/// Asks the node for `hash` like a peer would. Returns the file, or `None` for `NotOffered`.
async fn request(conn: &Connection, hash: ContentHash) -> Option<Vec<u8>> {
    let (mut send, mut recv) = conn.open_bi().await.expect("open stream");
    write(&mut send, &Message::FileRequest(FileRequest { hash })).await;
    send.finish().expect("finish");
    let all = time::timeout(TIMEOUT, recv.read_to_end(usize::MAX))
        .await
        .expect("answer in time")
        .expect("whole answer");
    let (message, used) = protocol::decode(&all)
        .expect("valid frame")
        .expect("whole frame");
    match message {
        Message::FileResponse(FileResponse::Ok { size }) => {
            assert_eq!(
                size,
                (all.len() - used) as u64,
                "exactly `size` bytes follow"
            );
            Some(all[used..].to_vec())
        }
        Message::FileResponse(FileResponse::NotOffered) => {
            assert_eq!(used, all.len(), "nothing follows NotOffered");
            None
        }
        other => panic!("unexpected answer {other:?}"),
    }
}

/// The raw side of a connection to a node. Keeps the clip streams open: a node closes the
/// connection when the peer finishes its clip stream.
struct RawPeer {
    spec: Spec,
    conn: Connection,
    _endpoint: Endpoint,
    _streams: (SendStream, RecvStream),
}

/// A node connected to a raw peer that it lets in.
async fn node_and_raw_peer() -> (Node, RawPeer) {
    let peer = Spec::new();
    let node_spec = Spec::new();
    let (node, _inbox) = node_spec.spawn(vec![peer.peer_without_addrs()]).await;
    let endpoint = raw_endpoint(&peer).await;
    let conn = raw_connect(&endpoint, &node_spec).await;
    let streams = raw_hello(&conn).await;
    wait_until("the node sees the peer", || {
        node.live_sessions_with(peer.id()) == 1
    })
    .await;
    let raw = RawPeer {
        spec: peer,
        conn,
        _endpoint: endpoint,
        _streams: streams,
    };
    (node, raw)
}

#[tokio::test]
async fn several_files_transfer_between_two_nodes() {
    let (a, b) = (Spec::new(), Spec::new());
    let (node_a, _inbox_a) = a.spawn(vec![b.peer()]).await;
    let (node_b, mut inbox_b) = b.spawn(vec![a.peer()]).await;
    wait_until("connected", || {
        node_a.connected_once_to(&[&b]) && node_b.connected_once_to(&[&a])
    })
    .await;

    let source = tempfile::tempdir().expect("temp dir");
    let big = random_bytes(40 << 20, 1);
    let contents: [(&str, &[u8]); 3] = [("big.bin", &big), ("notes.txt", b"hello"), ("empty", b"")];
    let config = Config::default();
    let paths = write_files(source.path(), &contents);
    let offer = Offer::read(&paths, &config)
        .expect("files readable")
        .expect("files to offer");
    let clip = files_clip(a.id(), 1, offer.list().clone());
    node_a.broadcast_files(clip, offer).expect("send");

    let received = time::timeout(TIMEOUT, inbox_b.recv())
        .await
        .expect("clip in time")
        .expect("node running");
    assert_eq!(received.from, a.id());
    let Ok(ClipContent::Files(list)) = received.clip.into_content() else {
        panic!("an intact files clip");
    };
    let cache_dir = tempfile::tempdir().expect("temp dir");
    let cache = open_cache(&cache_dir, &config);
    let fetched = time::timeout(BIG_TIMEOUT, node_b.fetch_files(a.id(), &list, &cache))
        .await
        .expect("fetched in time")
        .expect("fetched");

    assert_eq!(fetched.len(), 3);
    for (entry, path) in list.entries().iter().zip(&fetched) {
        assert_eq!(
            path,
            &cache.dir().join(entry.hash.to_string()).join(&entry.name)
        );
        let (_, original) = contents
            .iter()
            .find(|(name, _)| *name == entry.name)
            .expect("a sent file");
        let got = fs::read(path).expect("read cached file");
        assert_eq!(ContentHash::of(&got), entry.hash, "{}", entry.name);
        assert!(got == *original, "{} has the original bytes", entry.name);
    }
    assert_eq!(cached_files(&cache).len(), 3, "no temporary file left");

    // Files in the cache are not asked for again: `a` no longer serves them, yet this works.
    node_a
        .broadcast(text_clip(a.id(), 2, "something else"))
        .expect("send");
    let again = node_b.fetch_files(a.id(), &list, &cache).await;
    assert_eq!(again.expect("from the cache"), fetched);

    node_a.shutdown().await;
    node_b.shutdown().await;
}

#[tokio::test]
async fn only_files_of_the_latest_clip_are_served() {
    let (node, raw) = node_and_raw_peer().await;
    let conn = &raw.conn;
    let source = tempfile::tempdir().expect("temp dir");
    let paths = write_files(
        source.path(),
        &[("offered.txt", b"offered"), ("private.txt", b"not offered")],
    );
    let offer = Offer::read(&paths[..1], &Config::default())
        .expect("readable")
        .expect("one file");
    let clip = files_clip(node.id(), 1, offer.list().clone());
    node.broadcast_files(clip, offer).expect("send");

    let offered = ContentHash::of(b"offered");
    assert_eq!(request(conn, ContentHash::of(b"not offered")).await, None);
    assert_eq!(request(conn, offered).await, Some(b"offered".to_vec()));

    // A file that changed size after it was copied is not served.
    fs::write(&paths[0], b"offered, then edited").expect("edit");
    assert_eq!(request(conn, offered).await, None);

    // A newer clip withdraws the offer.
    fs::write(&paths[0], b"offered").expect("restore");
    assert_eq!(request(conn, offered).await, Some(b"offered".to_vec()));
    node.broadcast(text_clip(node.id(), 2, "text"))
        .expect("send");
    assert_eq!(request(conn, offered).await, None);

    node.shutdown().await;
}

#[tokio::test]
async fn bad_requests_are_dropped_and_the_connection_survives() {
    let (node, raw) = node_and_raw_peer().await;
    let source = tempfile::tempdir().expect("temp dir");
    let paths = write_files(source.path(), &[("a.txt", b"abc")]);
    let offer = Offer::read(&paths, &Config::default())
        .expect("readable")
        .expect("one file");
    let clip = files_clip(node.id(), 1, offer.list().clone());
    node.broadcast_files(clip, offer).expect("send");

    let hello = protocol::encode(&Message::Hello(Hello::CURRENT)).expect("encode");
    for junk in [vec![0xff; 4096], hello] {
        let (mut send, mut recv) = raw.conn.open_bi().await.expect("open stream");
        // The node may stop reading before all of it arrives.
        let _ = send.write_all(&junk).await;
        let _ = send.finish();
        let answer = time::timeout(TIMEOUT, recv.read_to_end(1024)).await;
        assert!(
            !matches!(answer, Ok(Ok(ref bytes)) if !bytes.is_empty()),
            "no answer to junk: {answer:?}"
        );
    }
    assert_eq!(
        request(&raw.conn, ContentHash::of(b"abc")).await,
        Some(b"abc".to_vec())
    );

    node.shutdown().await;
}

#[tokio::test]
async fn offer_skips_folders_and_files_over_the_limit() {
    let source = tempfile::tempdir().expect("temp dir");
    let mut paths = write_files(
        source.path(),
        &[
            ("ok.txt", b"ok"),
            ("huge.bin", &random_bytes((1 << 20) + 1, 4)),
        ],
    );
    let folder = source.path().join("folder");
    fs::create_dir(&folder).expect("mkdir");
    paths.push(folder);

    let offer = Offer::read(&paths, &small_limits())
        .expect("readable")
        .expect("one file left");
    let names: Vec<_> = offer.list().entries().iter().map(|e| &e.name).collect();
    assert_eq!(names, ["ok.txt"]);

    let none = Offer::read(&paths[1..], &small_limits()).expect("readable");
    assert!(none.is_none(), "nothing left to offer");
}

#[tokio::test]
async fn hostile_senders_never_leave_a_file_in_the_cache() {
    type Check = fn(&FetchError) -> bool;
    let (node, raw) = node_and_raw_peer().await;
    let (peer, conn) = (&raw.spec, &raw.conn);
    let cache_dir = tempfile::tempdir().expect("temp dir");
    let cache = open_cache(&cache_dir, &small_limits());
    let body = random_bytes(100_000, 2);
    let entry = entry("data.bin", &body);
    let list = one_file(&entry);
    let ok = response(FileResponse::Ok { size: entry.size });

    let cases: [(&str, Vec<u8>, Check); 6] = [
        (
            "other bytes of the right size",
            [ok.clone(), random_bytes(body.len(), 3)].concat(),
            |e| matches!(e, FetchError::Store(StoreError::HashMismatch)),
        ),
        (
            "the stream ends halfway",
            [&ok[..], &body[..body.len() / 2]].concat(),
            |e| matches!(e, FetchError::Store(StoreError::Truncated { .. })),
        ),
        (
            "more bytes than announced",
            [&ok[..], &body, b"and more"].concat(),
            |e| matches!(e, FetchError::Store(StoreError::TooManyBytes { .. })),
        ),
        (
            "another size than announced",
            response(FileResponse::Ok {
                size: entry.size + 1,
            }),
            |e| matches!(e, FetchError::SizeMismatch { .. }),
        ),
        ("not offered", response(FileResponse::NotOffered), |e| {
            matches!(e, FetchError::NotOffered)
        }),
        (
            "not a file response",
            protocol::encode(&Message::Hello(Hello::CURRENT)).expect("encode"),
            |e| matches!(e, FetchError::Protocol(_)),
        ),
    ];
    for (case, answer, expected) in cases {
        let serve = async {
            let (mut send, hash) = accept_request(conn).await;
            assert_eq!(hash, entry.hash);
            // Fails once the node stops reading, which is the point of some cases.
            let _ = send.write_all(&answer).await;
            let _ = send.finish();
            send
        };
        let fetch = time::timeout(TIMEOUT, node.fetch_files(peer.id(), &list, &cache));
        let (fetched, _send) = tokio::join!(fetch, serve);
        let error = fetched.expect("ends in time").expect_err(case);
        assert!(expected(&error), "{case}: {error}");
        assert_eq!(cached_files(&cache), Vec::<PathBuf>::new(), "{case}");
    }

    node.shutdown().await;
}

#[tokio::test]
async fn limits_are_checked_before_asking_for_anything() {
    let (node, raw) = node_and_raw_peer().await;
    let (peer, conn) = (&raw.spec, &raw.conn);
    let cache_dir = tempfile::tempdir().expect("temp dir");
    let cache = open_cache(&cache_dir, &small_limits());
    let too_big = FileEntry {
        size: (1 << 20) + 1,
        ..entry("big.bin", b"")
    };
    let together_too_big = FileList::new(
        ["a", "b", "c"]
            .map(|name| FileEntry {
                size: 1 << 20,
                ..entry(name, name.as_bytes())
            })
            .to_vec(),
    )
    .expect("valid list");

    let error = node
        .fetch_files(peer.id(), &one_file(&too_big), &cache)
        .await;
    assert!(
        matches!(error, Err(FetchError::FileTooLarge { .. })),
        "{error:?}"
    );
    let error = node.fetch_files(peer.id(), &together_too_big, &cache).await;
    assert!(
        matches!(error, Err(FetchError::ClipTooLarge { .. })),
        "{error:?}"
    );

    let asked = time::timeout(Duration::from_millis(300), conn.accept_bi()).await;
    assert!(asked.is_err(), "the node opened no stream");
    let unknown = node
        .fetch_files(Spec::new().id(), &one_file(&too_big), &cache)
        .await;
    assert!(
        matches!(unknown, Err(FetchError::UnknownPeer)),
        "{unknown:?}"
    );

    node.shutdown().await;
}

#[tokio::test]
async fn newer_clip_cancels_the_running_transfer() {
    let (node, raw) = node_and_raw_peer().await;
    let (peer, conn) = (&raw.spec, &raw.conn);
    let cache_dir = tempfile::tempdir().expect("temp dir");
    let cache = open_cache(&cache_dir, &small_limits());
    let first_body = random_bytes(1 << 20, 5);
    let first = entry("first.bin", &first_body);
    let second = entry("second.txt", b"the newer copy");

    // The sender of the first clip sends a part of the file, then stalls.
    let (first_list, second_list) = (one_file(&first), one_file(&second));
    let mut fetch_first = Box::pin(node.fetch_files(peer.id(), &first_list, &cache));
    let serve_part = async {
        let (mut send, _) = accept_request(conn).await;
        let ok = response(FileResponse::Ok { size: first.size });
        send.write_all(&[&ok[..], &first_body[..64 * 1024]].concat())
            .await
            .expect("write");
        send
    };
    let first_send = tokio::select! {
        _ = &mut fetch_first => panic!("the first transfer cannot finish"),
        send = serve_part => send,
    };
    // Let the fetch write what it got to its temporary file.
    let partial = time::timeout(Duration::from_millis(300), &mut fetch_first).await;
    assert!(partial.is_err(), "still running");
    assert_eq!(cached_files(&cache).len(), 1, "a temporary file meanwhile");

    // A newer clip arrives: the daemon drops the running fetch and starts the new one.
    drop(fetch_first);
    let stopped = time::timeout(TIMEOUT, first_send.stopped()).await;
    assert!(
        matches!(stopped, Ok(Ok(Some(_)))),
        "the sender is told to stop: {stopped:?}"
    );
    assert_eq!(
        cached_files(&cache),
        Vec::<PathBuf>::new(),
        "temp file deleted"
    );

    let serve_second = async {
        let (mut send, hash) = accept_request(conn).await;
        assert_eq!(hash, second.hash);
        let ok = response(FileResponse::Ok { size: second.size });
        send.write_all(&[&ok[..], b"the newer copy"].concat())
            .await
            .expect("write");
        send.finish().expect("finish");
        send
    };
    let fetch_second = node.fetch_files(peer.id(), &second_list, &cache);
    let (fetched, _send) = tokio::join!(time::timeout(TIMEOUT, fetch_second), serve_second);
    let paths = fetched.expect("in time").expect("fetched");
    assert_eq!(fs::read(&paths[0]).expect("read"), b"the newer copy");
    assert_eq!(cached_files(&cache), paths);

    node.shutdown().await;
}

#[tokio::test]
async fn transfer_is_retried_on_the_next_connection() {
    let peer = Spec::new();
    let node_spec = Spec::new();
    let (node, _inbox) = node_spec.spawn(vec![peer.peer_without_addrs()]).await;
    let raw = raw_endpoint(&peer).await;
    let first_conn = raw_connect(&raw, &node_spec).await;
    let _first_streams = raw_hello(&first_conn).await;
    wait_until("connected", || node.live_sessions_with(peer.id()) == 1).await;

    let cache_dir = tempfile::tempdir().expect("temp dir");
    let cache = open_cache(&cache_dir, &small_limits());
    let body = random_bytes(500_000, 6);
    let entry = entry("data.bin", &body);
    let list = one_file(&entry);
    let ok = response(FileResponse::Ok { size: entry.size });

    let peer_side = async {
        // Half the file, then the connection drops.
        let (mut send, _) = accept_request(&first_conn).await;
        let _ = send
            .write_all(&[&ok[..], &body[..body.len() / 2]].concat())
            .await;
        first_conn.close(VarInt::from_u32(0), b"gone");
        // The peer comes back on a new connection and is asked again.
        let conn = raw_connect(&raw, &node_spec).await;
        let streams = raw_hello(&conn).await;
        let (mut send, _) = accept_request(&conn).await;
        send.write_all(&[&ok[..], &body].concat())
            .await
            .expect("write");
        send.finish().expect("finish");
        (conn, streams, send)
    };
    let fetch = time::timeout(TIMEOUT, node.fetch_files(peer.id(), &list, &cache));
    let (fetched, _peer_side) = tokio::join!(fetch, peer_side);
    let paths = fetched
        .expect("in time")
        .expect("fetched on the second try");
    assert_eq!(
        ContentHash::of(&fs::read(&paths[0]).expect("read")),
        entry.hash
    );

    node.shutdown().await;
}

#[tokio::test]
async fn transfer_uses_the_connection_the_smaller_id_kept() {
    // After a duplicate dial, the node with the smaller id may keep the older connection and
    // close the newer one. The bigger node cannot predict which, so it must fetch on whichever
    // is still open, not on the one that greeted last.
    let (small, big) = Spec::ordered_pair();
    let (node, _inbox) = big.spawn(vec![small.peer_without_addrs()]).await;
    let raw = raw_endpoint(&small).await;
    let kept = raw_connect(&raw, &big).await;
    let _kept_streams = raw_hello(&kept).await;
    let dropped = raw_connect(&raw, &big).await;
    let _dropped_streams = raw_hello(&dropped).await;
    wait_until("both sessions are live", || {
        node.live_sessions_with(small.id()) == 2
    })
    .await;
    dropped.close(VarInt::from_u32(1), b"replaced by a newer connection");

    let cache_dir = tempfile::tempdir().expect("temp dir");
    let cache = open_cache(&cache_dir, &small_limits());
    let body = random_bytes(10_000, 7);
    let entry = entry("kept.bin", &body);
    let list = one_file(&entry);

    let peer_side = async {
        let (mut send, _) = accept_request(&kept).await;
        let ok = response(FileResponse::Ok { size: entry.size });
        send.write_all(&[&ok[..], &body].concat())
            .await
            .expect("write");
        send.finish().expect("finish");
        send
    };
    let fetch = time::timeout(TIMEOUT, node.fetch_files(small.id(), &list, &cache));
    let (fetched, _send) = tokio::join!(fetch, peer_side);
    let paths = fetched
        .expect("in time")
        .expect("fetched on the kept connection");
    assert_eq!(fs::read(&paths[0]).expect("read"), body);

    node.shutdown().await;
}
