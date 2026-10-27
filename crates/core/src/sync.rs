//! Sync engine: decides what to send to peers and what to apply locally, with no IO.
//!
//! The daemon keeps one [`SyncEngine`] and feeds it the two kinds of input it gets:
//!
//! 1. A local clipboard change event. The daemon reads the text and calls
//!    [`SyncEngine::local_changed`]. On [`LocalAction::Send`] it broadcasts the clip to every peer.
//! 2. A clip received from a peer. The daemon calls [`SyncEngine::remote_clip`]. On
//!    [`RemoteAction::Apply`] it writes the content to the local clipboard.
//!
//! The write in step 2 comes back as a local change event, because the `Clipboard` contract
//! reports the daemon's own writes. The engine recognises that content by hash and answers
//! [`LocalAction::Unchanged`], so it is not sent back and two machines never bounce it forever.
//!
//! The caller passes the current time in; the engine never reads a clock, so tests are
//! deterministic.
//!
//! ```
//! use clipbroad_core::ClipContent;
//! use clipbroad_core::protocol::NodeId;
//! use clipbroad_core::sync::{LocalAction, RemoteAction, SyncEngine};
//!
//! let mut laptop = SyncEngine::new(NodeId::from_bytes([1; 32]));
//! let mut desktop = SyncEngine::new(NodeId::from_bytes([2; 32]));
//!
//! // The user copies on the laptop: the laptop sends it.
//! let LocalAction::Send(clip) = laptop.local_changed(ClipContent::from("hi"), 1_000) else {
//!     panic!("a new copy is sent");
//! };
//! // The desktop receives it and writes it to its clipboard.
//! let RemoteAction::Apply(content) = desktop.remote_clip(clip)? else {
//!     panic!("a newer clip is applied");
//! };
//! // That write fires a change event on the desktop: it is the echo, so nothing is sent.
//! assert_eq!(desktop.local_changed(content, 1_005), LocalAction::Unchanged);
//! # Ok::<(), clipbroad_core::protocol::ClipError>(())
//! ```
//!
//! # Rules
//!
//! **The same content is not sent twice in a row.** The engine remembers the hash of the content
//! it last sent or applied, which is what the local clipboard holds. A local change with that hash
//! is ignored. It is the echo of an apply, a repeated event from the backend, or the user copying
//! the same text again. In all three cases the peers already have it: if one of them had copied
//! something newer, that clip would have arrived here and changed the remembered hash. So after
//! "A copies X, B copies Y, A copies X again", A sends X again and X wins everywhere.
//!
//! **The last copy wins.** Clips are ordered by their stamp, `(timestamp_ms, origin)`: the
//! timestamp first, the origin [`NodeId`] to break ties, so every machine picks the same winner.
//! A remote clip that is not newer than the newest stamp seen here is ignored.
//!
//! Plain clock timestamps are not enough, because clocks differ between machines. If B's clock is
//! 10 s behind A's, a copy on B made 3 s after a copy on A would carry an older timestamp and lose
//! everywhere, B included. So a local copy is stamped `max(now, newest seen + 1 ms)`: it always
//! beats every clip its machine has seen, whatever the clocks say. For one person copying on one
//! machine at a time, the last copy always wins.
//!
//! Known limits:
//! - Two copies on different machines, close enough that neither has seen the other's yet, are
//!   concurrent. The higher stamp wins, which favours the machine with the clock ahead. All
//!   machines still end up with the same content.
//! - A clock that is ahead pushes everyone's timestamps ahead too. Ordering still works, but
//!   `timestamp_ms` stops being exact wall-clock time.
//! - State lives only in memory. A freshly started engine has seen nothing, so while a peer's
//!   clock is ahead of its own, its first copy can lose to the last clip that peer stamped. This
//!   lasts at most the clock difference.
//! - Peers are not caught up when they reconnect. A peer that missed a clip keeps its old content
//!   until the next copy, and copying the same text again does not resend it.

use crate::protocol::{Clip, ClipError, ClipId, NodeId};
use crate::{ClipContent, ContentHash};

/// What the daemon does after a local clipboard change.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    /// New content: broadcast this clip to every peer.
    Send(Clip),
    /// The clipboard already held this content as far as sync knows (an echo of an apply, a
    /// repeated event, or the same text copied again). Do nothing.
    Unchanged,
}

/// What the daemon does after receiving a clip from a peer.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    /// Newer content: write it to the local clipboard.
    Apply(ClipContent),
    /// Newer clip, but the local clipboard already holds this content. Do nothing.
    AlreadyHave,
    /// Older than (or the same clip as) the newest one seen here. Do nothing.
    Stale,
}

/// Sync state of one node. Its size is fixed: it keeps no history.
#[derive(Debug)]
pub struct SyncEngine {
    node: NodeId,
    next_id: u64,
    /// Stamp of the newest clip sent or applied here.
    newest: Option<Stamp>,
    /// Hash of the content last sent or applied, which is what the local clipboard holds.
    current: Option<ContentHash>,
}

/// Order of clips: the timestamp first, the origin to break ties. Field order is the comparison
/// order of the derived `Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    timestamp_ms: u64,
    origin: NodeId,
}

impl SyncEngine {
    /// An engine for the node `node`, which has seen nothing yet.
    #[must_use]
    pub const fn new(node: NodeId) -> Self {
        Self {
            node,
            next_id: 0,
            newest: None,
            current: None,
        }
    }

    /// The local clipboard changed and now holds `content`. `now_ms` is the local clock, in
    /// milliseconds since the Unix epoch.
    pub fn local_changed(&mut self, content: ClipContent, now_ms: u64) -> LocalAction {
        let hash = content.hash();
        if self.current == Some(hash) {
            return LocalAction::Unchanged;
        }
        // Past everything seen here, so this copy wins even if a peer's clock is ahead.
        let timestamp_ms = self.newest.map_or(now_ms, |newest| {
            now_ms.max(newest.timestamp_ms.saturating_add(1))
        });
        let id = ClipId(self.next_id);
        self.next_id += 1;
        self.newest = Some(Stamp {
            timestamp_ms,
            origin: self.node,
        });
        self.current = Some(hash);
        LocalAction::Send(Clip::new(id, self.node, timestamp_ms, content))
    }

    /// A peer sent `clip`.
    ///
    /// # Errors
    ///
    /// [`ClipError`] if the clip's payload does not match its declared size and hash (see
    /// [`Clip::into_content`]). The engine's state does not change.
    pub fn remote_clip(&mut self, clip: Clip) -> Result<RemoteAction, ClipError> {
        let stamp = Stamp {
            timestamp_ms: clip.timestamp_ms,
            origin: clip.origin,
        };
        if self.newest.is_some_and(|newest| stamp <= newest) {
            return Ok(RemoteAction::Stale);
        }
        let hash = clip.hash;
        let content = clip.into_content()?;
        self.newest = Some(stamp);
        if self.current.replace(hash) == Some(hash) {
            return Ok(RemoteAction::AlreadyHave);
        }
        Ok(RemoteAction::Apply(content))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::content::tiny_image;
    use crate::protocol::Payload;

    fn node(n: u8) -> NodeId {
        NodeId::from_bytes([n; 32])
    }

    fn text(s: &str) -> ClipContent {
        ClipContent::from(s)
    }

    fn sent(action: LocalAction) -> Clip {
        match action {
            LocalAction::Send(clip) => clip,
            LocalAction::Unchanged => panic!("expected Send, got Unchanged"),
        }
    }

    /// Machines wired together the way the daemon wires an engine to a clipboard and the network.
    /// Every write to a clipboard queues a change event, like a real backend.
    struct Mesh {
        machines: Vec<Machine>,
        events: VecDeque<Event>,
        /// Shared "real" time; each machine adds its clock offset.
        now_ms: u64,
        /// Clips broadcast so far (one per `Send`, whatever the number of peers).
        sends: usize,
    }

    struct Machine {
        engine: SyncEngine,
        clipboard: Option<ClipContent>,
        clock_offset_ms: i64,
    }

    enum Event {
        LocalChange { at: usize },
        Deliver { to: usize, clip: Clip },
    }

    impl Mesh {
        fn new(count: u8) -> Self {
            let machines = (1..=count)
                .map(|n| Machine {
                    engine: SyncEngine::new(node(n)),
                    clipboard: None,
                    clock_offset_ms: 0,
                })
                .collect();
            Self {
                machines,
                events: VecDeque::new(),
                now_ms: 1_700_000_000_000,
                sends: 0,
            }
        }

        fn with_clock_offset(mut self, at: usize, offset_ms: i64) -> Self {
            self.machines[at].clock_offset_ms = offset_ms;
            self
        }

        fn wait(&mut self, ms: u64) {
            self.now_ms += ms;
        }

        /// The user copies `s` on machine `at`. Nothing is processed until [`Self::run`].
        fn copy(&mut self, at: usize, s: &str) {
            self.write_clipboard(at, text(s));
        }

        fn write_clipboard(&mut self, at: usize, content: ClipContent) {
            self.machines[at].clipboard = Some(content);
            self.events.push_back(Event::LocalChange { at });
        }

        /// Processes events until none are left. Panics if they never stop (an echo loop).
        fn run(&mut self) {
            for _ in 0..1_000 {
                let Some(event) = self.events.pop_front() else {
                    return;
                };
                match event {
                    Event::LocalChange { at } => self.local_change(at),
                    Event::Deliver { to, clip } => self.deliver(to, clip),
                }
            }
            panic!("events never stopped: echo loop");
        }

        fn local_change(&mut self, at: usize) {
            let machine = &mut self.machines[at];
            // The daemon reads the clipboard when it handles the event, so it sees what is there now.
            let content = machine
                .clipboard
                .clone()
                .expect("a change event has content");
            let now_ms = self.now_ms.saturating_add_signed(machine.clock_offset_ms);
            if let LocalAction::Send(clip) = machine.engine.local_changed(content, now_ms) {
                self.sends += 1;
                for to in (0..self.machines.len()).filter(|&to| to != at) {
                    self.events.push_back(Event::Deliver {
                        to,
                        clip: clip.clone(),
                    });
                }
            }
        }

        fn deliver(&mut self, to: usize, clip: Clip) {
            let action = self.machines[to]
                .engine
                .remote_clip(clip)
                .expect("intact clip");
            if let RemoteAction::Apply(content) = action {
                self.write_clipboard(to, content);
            }
        }

        fn clipboards(&self) -> Vec<Option<&str>> {
            self.machines
                .iter()
                .map(|m| m.clipboard.as_ref().and_then(ClipContent::as_text))
                .collect()
        }
    }

    #[test]
    fn a_copy_reaches_every_machine_without_echo_loop() {
        let mut mesh = Mesh::new(3);
        mesh.copy(0, "hello");
        mesh.run();
        assert_eq!(mesh.clipboards(), [Some("hello"); 3]);
        // Only the original copy was sent: the applies on the other two did not bounce back.
        assert_eq!(mesh.sends, 1);
    }

    #[test]
    fn copies_on_different_machines_take_turns() {
        let mut mesh = Mesh::new(3);
        for (at, s) in [(0, "one"), (1, "two"), (2, "three"), (0, "four")] {
            mesh.copy(at, s);
            mesh.run();
            assert_eq!(mesh.clipboards(), [Some(s); 3]);
            mesh.wait(2_000);
        }
        assert_eq!(mesh.sends, 4);
    }

    #[test]
    fn same_text_copied_twice_is_sent_once() {
        let mut mesh = Mesh::new(2);
        mesh.copy(0, "x");
        mesh.run();
        mesh.wait(1_000);
        mesh.copy(0, "x");
        mesh.run();
        assert_eq!(mesh.clipboards(), [Some("x"); 2]);
        assert_eq!(mesh.sends, 1);
    }

    #[test]
    fn copying_again_after_another_machine_copied_wins_everywhere() {
        // A copies X, B copies Y, A copies X again: X must end up everywhere.
        let mut mesh = Mesh::new(3);
        for (at, s) in [(0, "x"), (1, "y"), (0, "x")] {
            mesh.copy(at, s);
            mesh.run();
            mesh.wait(1_000);
        }
        assert_eq!(mesh.clipboards(), [Some("x"); 3]);
        assert_eq!(mesh.sends, 3);
    }

    #[test]
    fn machine_with_clock_behind_still_wins_with_a_later_copy() {
        // B's clock is 10 s behind. With plain timestamps its copy, made 3 s after A's, would lose.
        let mut mesh = Mesh::new(3).with_clock_offset(1, -10_000);
        mesh.copy(0, "from a");
        mesh.run();
        mesh.wait(3_000);
        mesh.copy(1, "from b");
        mesh.run();
        assert_eq!(mesh.clipboards(), [Some("from b"); 3]);
    }

    #[test]
    fn concurrent_copies_converge_on_the_same_content() {
        // Same millisecond, before either copy is delivered: the higher origin wins the tie.
        let mut mesh = Mesh::new(3);
        mesh.copy(0, "from a");
        mesh.copy(1, "from b");
        mesh.run();
        assert_eq!(mesh.clipboards(), [Some("from b"); 3]);

        // With A's clock ahead, A's copy has the higher stamp, even though B would win a tie.
        let mut mesh = Mesh::new(3).with_clock_offset(0, 500);
        mesh.copy(0, "from a");
        mesh.copy(1, "from b");
        mesh.run();
        assert_eq!(mesh.clipboards(), [Some("from a"); 3]);
    }

    #[test]
    fn images_sync_like_text() {
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let clip = sent(a.local_changed(tiny_image(), 1_000));
        let RemoteAction::Apply(content) = b.remote_clip(clip).expect("intact") else {
            panic!("expected Apply");
        };
        assert_eq!(content, tiny_image());
        // The write on B comes back as a change event: the echo is not sent.
        assert_eq!(b.local_changed(content, 1_005), LocalAction::Unchanged);

        // A later text copy on B replaces the image on A.
        let clip = sent(b.local_changed(text("after"), 2_000));
        assert_eq!(a.remote_clip(clip), Ok(RemoteAction::Apply(text("after"))));
    }

    #[test]
    fn out_of_order_delivery_keeps_the_newest() {
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let older = sent(a.local_changed(text("older"), 1_000));
        let newer = sent(a.local_changed(text("newer"), 2_000));

        assert_eq!(b.remote_clip(newer), Ok(RemoteAction::Apply(text("newer"))));
        assert_eq!(b.remote_clip(older), Ok(RemoteAction::Stale));
    }

    #[test]
    fn duplicate_delivery_is_stale() {
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let clip = sent(a.local_changed(text("x"), 1_000));
        assert_eq!(
            b.remote_clip(clip.clone()),
            Ok(RemoteAction::Apply(text("x")))
        );
        assert_eq!(b.remote_clip(clip), Ok(RemoteAction::Stale));
    }

    #[test]
    fn repeated_echo_events_are_all_ignored() {
        // Some backends (Windows) report a single write more than once.
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let clip = sent(a.local_changed(text("x"), 1_000));
        let RemoteAction::Apply(content) = b.remote_clip(clip).expect("intact") else {
            panic!("expected Apply");
        };
        for now_ms in [1_001, 1_002, 1_003] {
            assert_eq!(
                b.local_changed(content.clone(), now_ms),
                LocalAction::Unchanged
            );
        }
    }

    #[test]
    fn newer_clip_with_content_already_held_is_not_written() {
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let _ = b.local_changed(text("x"), 1_000);
        let clip = sent(a.local_changed(text("x"), 2_000));
        assert_eq!(b.remote_clip(clip), Ok(RemoteAction::AlreadyHave));
    }

    #[test]
    fn local_stamp_goes_past_a_clock_ahead() {
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let ahead = sent(a.local_changed(text("x"), 50_000));
        let _ = b.remote_clip(ahead).expect("intact");
        let clip = sent(b.local_changed(text("y"), 10_000));
        assert_eq!(clip.timestamp_ms, 50_001);
    }

    #[test]
    fn tampered_clip_is_rejected_and_changes_nothing() {
        let mut a = SyncEngine::new(node(1));
        let mut b = SyncEngine::new(node(2));
        let mut bad = sent(a.local_changed(text("x"), 2_000));
        bad.payload = Payload::Inline(b"y".to_vec());
        assert_eq!(b.remote_clip(bad), Err(ClipError::HashMismatch));

        // An older valid clip is still accepted: the bad one did not move the newest stamp.
        let older = Clip::new(ClipId(0), node(3), 1_000, text("z"));
        assert_eq!(b.remote_clip(older), Ok(RemoteAction::Apply(text("z"))));
    }
}
