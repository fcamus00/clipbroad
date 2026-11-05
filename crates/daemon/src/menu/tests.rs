use clipbroad_core::protocol::ContentKind;
use iroh::{EndpointId, SecretKey};

use super::*;
use crate::status::{ClipEvent, Mdns, PeerStatus};
use crate::update::Version;

const NOW_MS: u64 = 1_000_000_000;

fn id(byte: u8) -> EndpointId {
    SecretKey::from_bytes(&[byte; 32]).public()
}

fn peer(byte: u8, name: &str, connected: bool) -> PeerStatus {
    PeerStatus {
        id: id(byte),
        name: Some(name.to_owned()),
        connected,
        addr: None,
        added_by: None,
    }
}

fn status(peers: Vec<PeerStatus>) -> DaemonStatus {
    DaemonStatus {
        pid: 1,
        updated_ms: NOW_MS,
        node_id: id(1),
        port: 4747,
        mdns: Mdns::On,
        paste_access: None,
        paused: false,
        update: None,
        last_sent: None,
        last_received: None,
        peers,
    }
}

fn clip(at_ms: u64, from: Option<EndpointId>) -> ClipEvent {
    ClipEvent {
        at_ms,
        kind: ContentKind::Text,
        size: 120,
        files: None,
        hash: "1a2b3c4d".to_owned(),
        from,
    }
}

#[test]
fn counts_connected_machines() {
    let view = View::new(
        &status(vec![peer(2, "laptop", true), peer(3, "mac", false)]),
        None,
        false,
        State::Idle,
        NOW_MS,
    );
    assert_eq!(view.status, "1 of 2 machines connected");
    assert!(view.syncing);
    assert!(view.can_pair);
}

#[test]
fn does_not_count_as_syncing_without_a_connected_peer() {
    let alone = View::new(&status(vec![]), None, false, State::Idle, NOW_MS);
    assert_eq!(alone.status, "No machines paired yet");
    assert!(!alone.syncing);
    let offline = View::new(
        &status(vec![peer(2, "laptop", false)]),
        None,
        false,
        State::Idle,
        NOW_MS,
    );
    assert_eq!(offline.status, "0 of 1 machine connected");
    assert!(!offline.syncing);
}

#[test]
fn does_not_count_as_syncing_while_paused() {
    let paused = DaemonStatus {
        paused: true,
        ..status(vec![peer(2, "laptop", true)])
    };
    let view = View::new(&paused, None, false, State::Idle, NOW_MS);
    assert_eq!(view.status, "Sync paused. 1 of 1 machine connected");
    assert!(view.paused);
    assert!(!view.syncing);
}

#[test]
fn shows_the_newest_clip() {
    let clips = DaemonStatus {
        last_sent: Some(clip(NOW_MS - 3 * 60_000, None)),
        last_received: Some(clip(NOW_MS - 5_000, Some(id(2)))),
        ..status(vec![peer(2, "laptop", true)])
    };
    assert_eq!(
        View::new(&clips, None, false, State::Idle, NOW_MS).activity,
        "Last received from laptop: text, 120 bytes, 5 s ago"
    );
    let older_received = DaemonStatus {
        last_sent: Some(clip(NOW_MS - 5_000, None)),
        last_received: Some(clip(NOW_MS - 3 * 60_000, Some(id(2)))),
        ..clips
    };
    assert_eq!(
        View::new(&older_received, None, false, State::Idle, NOW_MS).activity,
        "Last sent: text, 120 bytes, 5 s ago"
    );
    assert_eq!(
        View::new(&status(vec![]), None, false, State::Idle, NOW_MS).activity,
        "No clips yet"
    );
}

#[test]
fn a_note_replaces_the_last_clip_until_it_expires() {
    let status = DaemonStatus {
        last_sent: Some(clip(NOW_MS - 5_000, None)),
        ..status(vec![])
    };
    let (mut menu, gui) = channel();
    menu.note("Paired", NOW_MS);
    menu.show(&status, false, State::Idle, NOW_MS);
    assert_eq!(gui.views.try_recv().expect("sent").activity, "Paired");
    menu.show(&status, false, State::Idle, NOW_MS + NOTE_TTL_MS);
    assert_eq!(
        gui.views.try_recv().expect("sent").activity,
        format!("Last sent: text, 120 bytes, {} min ago", 5)
    );
}

#[test]
fn sends_a_view_only_when_it_changes() {
    let status = status(vec![]);
    let (mut menu, gui) = channel();
    menu.show(&status, false, State::Idle, NOW_MS);
    menu.show(&status, false, State::Idle, NOW_MS + 1);
    assert!(gui.views.try_recv().is_ok());
    assert!(gui.views.try_recv().is_err());
    menu.show(&status, true, State::Idle, NOW_MS + 2);
    assert!(!gui.views.try_recv().expect("changed").can_pair);
}

#[test]
fn works_without_an_icon() {
    let (mut menu, gui) = channel();
    drop(gui);
    menu.show(&status(vec![]), false, State::Idle, NOW_MS);
}

fn update_item(state: State, paused: bool, pairing: bool) -> (String, bool) {
    let status = DaemonStatus {
        paused,
        ..status(vec![])
    };
    let UpdateItem { text, enabled } = View::new(&status, None, pairing, state, NOW_MS).update;
    (text, enabled)
}

#[test]
fn update_item_follows_the_update() {
    let version: Version = "9.8.7".parse().expect("a version");
    assert_eq!(
        update_item(State::Idle, false, false),
        (format!("Check for updates ({VERSION})"), true)
    );
    assert_eq!(
        update_item(State::Checking, false, false),
        ("Checking for updates...".to_owned(), false)
    );
    assert_eq!(
        update_item(State::Available(version), false, false),
        ("Update to 9.8.7 and restart".to_owned(), true)
    );
    assert_eq!(
        update_item(State::Installing(version), false, false),
        ("Updating to 9.8.7...".to_owned(), false)
    );
}

#[test]
fn nothing_installs_while_a_restart_would_get_in_the_way() {
    let available = State::Available("9.8.7".parse().expect("a version"));
    // A restart resumes sync, which would undo the pause.
    assert_eq!(
        update_item(available, true, false),
        ("Resume sync to update".to_owned(), false)
    );
    assert_eq!(
        update_item(available, false, true),
        ("Update to 9.8.7 and restart".to_owned(), false)
    );
    // Only installing is blocked: looking for a version restarts nothing.
    assert!(update_item(State::Idle, true, true).1);
}
