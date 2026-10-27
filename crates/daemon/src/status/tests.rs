use std::net::Ipv4Addr;

use iroh::SecretKey;

use super::*;

/// Some day at 12:00:00 UTC.
const NOW_MS: u64 = 1_761_566_400_000;

fn id(seed: u8) -> EndpointId {
    SecretKey::from_bytes(&[seed; 32]).public()
}

fn sample() -> DaemonStatus {
    DaemonStatus {
        pid: 4242,
        updated_ms: NOW_MS - 2_000,
        node_id: id(1),
        port: 4747,
        mdns: Mdns::On,
        paste_access: None,
        last_sent: Some(ClipEvent {
            at_ms: NOW_MS - 3 * 60_000,
            kind: ContentKind::Text,
            size: 120,
            hash: "1a2b3c4d".to_owned(),
            from: None,
        }),
        last_received: None,
        peers: vec![
            PeerStatus {
                id: id(2),
                name: Some("laptop".to_owned()),
                connected: true,
                addr: Some((Ipv4Addr::new(192, 168, 1, 20), 4747).into()),
            },
            PeerStatus {
                id: id(3),
                name: None,
                connected: false,
                addr: None,
            },
        ],
    }
}

#[test]
fn shows_a_running_daemon_aligned() {
    let expected = format!(
        "\
daemon         running (pid 4242)
node id        {node}
port           UDP 4747
LAN discovery  on (mDNS)
peers          1 of 2 connected
               laptop     {laptop}  connected via 192.168.1.20:4747
               (no name)  {other}  not connected
last sent      11:57:00 UTC (3 min ago), text, 120 bytes, hash 1a2b3c4d
last received  nothing yet
",
        node = id(1),
        laptop = id(2).fmt_short(),
        other = id(3).fmt_short(),
    );
    assert_eq!(render(Some(&sample()), NOW_MS), expected);
}

#[test]
fn names_the_peer_a_clip_came_from() {
    let status = DaemonStatus {
        last_received: Some(ClipEvent {
            at_ms: NOW_MS - 5_000,
            kind: ContentKind::Image,
            size: 48_213,
            hash: "9f8e7d6c".to_owned(),
            from: Some(id(2)),
        }),
        ..sample()
    };
    assert!(render(Some(&status), NOW_MS).contains(
        "last received  11:59:55 UTC (5 s ago), image, 48213 bytes, hash 9f8e7d6c, from laptop\n"
    ));
}

#[test]
fn a_missing_or_stale_file_means_not_running() {
    assert!(render(None, NOW_MS).starts_with("daemon         not running\n"));
    let stale = DaemonStatus {
        updated_ms: NOW_MS - 60 * 60_000,
        ..sample()
    };
    assert!(render(Some(&stale), NOW_MS).starts_with(
        "daemon         not running: it did not stop cleanly, last seen at 11:00:00 UTC (1 h ago)\n"
    ));
}

#[test]
fn explains_how_to_allow_macos_pasteboard_reads() {
    let fix = "Paste from Other Apps and set clipbroad to Allow";
    let with = |access| {
        let status = DaemonStatus {
            paste_access: Some(access),
            ..sample()
        };
        render(Some(&status), NOW_MS)
    };
    assert!(with(PasteAccess::AlwaysAllow).contains("paste access   allowed\n"));
    assert!(!with(PasteAccess::AlwaysAllow).contains(fix));
    for access in [
        PasteAccess::Default,
        PasteAccess::Ask,
        PasteAccess::AlwaysDeny,
        PasteAccess::Other(7),
    ] {
        assert!(with(access).contains(fix), "{access:?}");
    }
    // Other OSes have no such setting, so no line at all.
    assert!(!render(Some(&sample()), NOW_MS).contains("paste access"));
}

#[test]
fn every_field_survives_the_file_format() {
    let status = DaemonStatus {
        paste_access: Some(PasteAccess::Other(7)),
        last_received: Some(ClipEvent {
            at_ms: NOW_MS,
            kind: ContentKind::Image,
            size: 5,
            hash: "9f8e7d6c".to_owned(),
            from: Some(id(2)),
        }),
        ..sample()
    };
    let text = toml::to_string(&status).expect("serializes");
    assert_eq!(
        toml::from_str::<DaemonStatus>(&text).expect("parses"),
        status
    );
}

#[test]
fn save_replaces_the_file_and_remove_deletes_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("status.toml");
    assert_eq!(read(&path).expect("no file is fine"), None);

    let mut status = sample();
    status.save(&path);
    assert!(status.updated_ms >= NOW_MS, "save sets the time");
    assert_eq!(read(&path).expect("readable"), Some(status.clone()));
    status.last_received = status.last_sent.clone();
    status.save(&path);
    assert_eq!(read(&path).expect("readable"), Some(status));

    remove(&path);
    assert_eq!(read(&path).expect("no file is fine"), None);
}
