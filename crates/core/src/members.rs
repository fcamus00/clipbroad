//! Who is in the mesh: every machine that was paired with some member, and the ones that were
//! removed.
//!
//! Each node keeps its own [`Members`] and sends it to every peer it connects to. A node that
//! receives a list [merges](Members::merge) it into its own: it adds the machines it did not know
//! and removes the ones that were removed elsewhere. The merge gives the same result in any
//! order and repeating it changes nothing, so every node that keeps meeting the others ends up
//! with the same list.
//!
//! Removal is permanent. A removed machine never becomes a member again under the same id, even
//! if a node that has not heard of the removal yet still lists it. To join again, it needs a new
//! identity.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::protocol::NodeId;

/// Most entries a list holds, removed ones included. A merge adds no new entry past it.
pub const MAX_MEMBERS: usize = 256;

/// A machine in the mesh, or one that was removed from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: NodeId,
    /// Its hostname, for logs and `clipbroad status`.
    pub name: Option<String>,
    /// Where to dial it, as the member that paired with it saw it. Empty once removed.
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
    /// The member that paired with it, or `None` for a peer written in the config file.
    #[serde(default)]
    pub added_by: Option<NodeId>,
    #[serde(default)]
    pub removed: bool,
}

/// How a [`Members`] list changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A new member: sync with it.
    Added(NodeId),
    /// Removed: stop syncing with it. May name a machine this node never knew.
    Removed(NodeId),
}

/// Why [`Members::admit`] refused a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this machine was removed from the mesh; to join again it needs a new identity")]
pub struct WasRemoved;

/// The members of the mesh that one node knows, in the order it learned them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Members {
    #[serde(default)]
    members: Vec<Member>,
}

impl Members {
    /// Every entry, removed ones included.
    pub fn iter(&self) -> impl Iterator<Item = &Member> {
        self.members.iter()
    }

    /// The entries that are not removed.
    pub fn active(&self) -> impl Iterator<Item = &Member> {
        self.iter().filter(|member| !member.removed)
    }

    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&Member> {
        self.iter().find(|member| member.id == id)
    }

    fn get_mut(&mut self, id: NodeId) -> Option<&mut Member> {
        self.members.iter_mut().find(|member| member.id == id)
    }

    /// Adds a machine that was just paired. Returns `false`, and changes nothing, if it is a
    /// member already.
    ///
    /// # Errors
    ///
    /// [`WasRemoved`] if it was removed.
    pub fn admit(&mut self, member: Member) -> Result<bool, WasRemoved> {
        match self.get(member.id) {
            Some(known) if known.removed => Err(WasRemoved),
            Some(_) => Ok(false),
            None => {
                self.members.push(Member {
                    removed: false,
                    ..member
                });
                Ok(true)
            }
        }
    }

    /// Adds or updates a peer from the config file: its name and addresses are the ones the
    /// person wrote. A removed one stays removed. Returns whether anything changed.
    pub fn configure(&mut self, member: Member) -> bool {
        match self.get_mut(member.id) {
            Some(known) if known.removed => false,
            Some(known) if known.name == member.name && known.addrs == member.addrs => false,
            Some(known) => {
                known.name = member.name;
                known.addrs = member.addrs;
                true
            }
            None => {
                self.members.push(Member {
                    removed: false,
                    ..member
                });
                true
            }
        }
    }

    /// Removes `id` for good. Returns `false` if it was not an active member.
    pub fn remove(&mut self, id: NodeId) -> bool {
        match self.get_mut(id) {
            Some(known) if !known.removed => {
                known.removed = true;
                known.addrs.clear();
                true
            }
            _ => false,
        }
    }

    /// Merges the list `from` sent, as seen by `me`, and returns what changed.
    ///
    /// Entries about `me` or about `from` itself are ignored: a node does not take others' word
    /// about itself, and a peer cannot vouch for itself (a removed machine could otherwise list
    /// itself back in). Known members keep their name and addresses.
    pub fn merge(&mut self, me: NodeId, from: NodeId, incoming: Self) -> Vec<Change> {
        let mut changes = Vec::new();
        for mut member in incoming.members {
            if member.id == me || member.id == from {
                continue;
            }
            let full = self.members.len() >= MAX_MEMBERS;
            match self.get_mut(member.id) {
                Some(known) if member.removed && !known.removed => {
                    known.removed = true;
                    known.addrs.clear();
                    changes.push(Change::Removed(member.id));
                }
                Some(_) => {}
                None if full => {}
                None => {
                    changes.push(if member.removed {
                        member.addrs.clear();
                        Change::Removed(member.id)
                    } else {
                        Change::Added(member.id)
                    });
                    self.members.push(member);
                }
            }
        }
        changes
    }

    /// Drops the entries `keep` rejects, such as ids that are not valid keys.
    pub fn retain(&mut self, keep: impl FnMut(&Member) -> bool) {
        self.members.retain(keep);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: NodeId = NodeId::from_bytes([0; 32]);
    const PC1: NodeId = NodeId::from_bytes([1; 32]);
    const PC2: NodeId = NodeId::from_bytes([2; 32]);
    const PC3: NodeId = NodeId::from_bytes([3; 32]);

    fn member(id: NodeId, name: &str) -> Member {
        Member {
            id,
            name: Some(name.to_owned()),
            addrs: vec!["192.168.1.20:42420".parse().expect("valid")],
            added_by: Some(PC1),
            removed: false,
        }
    }

    fn removed(id: NodeId) -> Member {
        Member {
            removed: true,
            ..member(id, "gone")
        }
    }

    fn list(members: impl IntoIterator<Item = Member>) -> Members {
        Members {
            members: members.into_iter().collect(),
        }
    }

    fn active_ids(members: &Members) -> Vec<NodeId> {
        members.active().map(|member| member.id).collect()
    }

    #[test]
    fn merge_adds_the_members_this_node_did_not_know() {
        let mut mine = list([member(PC1, "pc1")]);
        let changes = mine.merge(ME, PC1, list([member(PC2, "pc2"), member(PC3, "pc3")]));
        assert_eq!(changes, [Change::Added(PC2), Change::Added(PC3)]);
        assert_eq!(active_ids(&mine), [PC1, PC2, PC3]);
        assert_eq!(mine.get(PC2), Some(&member(PC2, "pc2")));
    }

    #[test]
    fn merging_the_same_list_again_changes_nothing() {
        let mut mine = list([member(PC1, "pc1")]);
        let theirs = list([member(PC2, "pc2"), removed(PC3)]);
        assert_eq!(mine.merge(ME, PC1, theirs.clone()).len(), 2);
        let before = mine.clone();
        assert_eq!(mine.merge(ME, PC1, theirs), []);
        assert_eq!(mine, before);
    }

    #[test]
    fn merge_ignores_entries_about_me_and_about_the_sender() {
        let mut mine = list([member(PC1, "pc1")]);
        let theirs = list([removed(ME), removed(PC1), member(PC2, "pc2")]);
        // A removed sender listing itself as a member again is ignored the same way.
        let changes = mine.merge(ME, PC1, theirs);
        assert_eq!(changes, [Change::Added(PC2)]);
        assert_eq!(active_ids(&mine), [PC1, PC2]);
        assert!(mine.get(ME).is_none());
    }

    #[test]
    fn removal_spreads_and_is_permanent() {
        let mut mine = list([member(PC1, "pc1"), member(PC3, "pc3")]);
        assert_eq!(
            mine.merge(ME, PC1, list([removed(PC3)])),
            [Change::Removed(PC3)]
        );
        assert_eq!(active_ids(&mine), [PC1]);
        assert_eq!(mine.get(PC3).map(|m| m.addrs.len()), Some(0));

        // A peer that has not heard of the removal yet does not bring it back.
        let mut late = list([member(PC1, "pc1")]);
        assert_eq!(late.merge(ME, PC2, list([member(PC3, "pc3")])).len(), 1);
        assert_eq!(mine.merge(ME, PC2, late), []);
        assert_eq!(active_ids(&mine), [PC1]);

        // Neither pairing nor the config file does.
        assert_eq!(mine.admit(member(PC3, "pc3")), Err(WasRemoved));
        assert!(!mine.configure(member(PC3, "pc3")));
        assert_eq!(active_ids(&mine), [PC1]);
    }

    #[test]
    fn merge_order_does_not_matter() {
        let a = list([member(PC2, "pc2")]);
        let b = list([removed(PC2), member(PC3, "pc3")]);
        let (mut first, mut second) = (Members::default(), Members::default());
        first.merge(ME, PC1, a.clone());
        first.merge(ME, PC1, b.clone());
        second.merge(ME, PC1, b);
        second.merge(ME, PC1, a);
        assert_eq!(active_ids(&first), [PC3]);
        assert_eq!(active_ids(&second), [PC3]);
        assert!(first.get(PC2).is_some_and(|m| m.removed));
    }

    #[test]
    fn a_removal_of_an_unknown_machine_is_kept() {
        let mut mine = Members::default();
        assert_eq!(
            mine.merge(ME, PC1, list([removed(PC3)])),
            [Change::Removed(PC3)]
        );
        // So it cannot join later through another peer that still lists it.
        assert_eq!(mine.merge(ME, PC2, list([member(PC3, "pc3")])), []);
        assert_eq!(active_ids(&mine), []);
    }

    #[test]
    fn known_members_keep_their_details() {
        let mut mine = list([member(PC2, "pc2")]);
        let renamed = Member {
            addrs: Vec::new(),
            ..member(PC2, "other name")
        };
        assert_eq!(mine.merge(ME, PC1, list([renamed])), []);
        assert_eq!(mine.get(PC2), Some(&member(PC2, "pc2")));
    }

    #[test]
    fn admit_adds_once() {
        let mut mine = Members::default();
        assert_eq!(mine.admit(member(PC2, "pc2")), Ok(true));
        assert_eq!(mine.admit(member(PC2, "again")), Ok(false));
        assert_eq!(mine.get(PC2), Some(&member(PC2, "pc2")));
    }

    #[test]
    fn configure_follows_the_config_file() {
        let mut mine = list([member(PC2, "pc2")]);
        let edited = Member {
            addrs: vec!["100.64.0.2:42420".parse().expect("valid")],
            ..member(PC2, "desk")
        };
        assert!(mine.configure(edited.clone()));
        assert!(!mine.configure(edited.clone()));
        let saved = mine.get(PC2).expect("still there");
        assert_eq!((&saved.name, &saved.addrs), (&edited.name, &edited.addrs));
        // Who added it is kept.
        assert_eq!(saved.added_by, Some(PC1));
    }

    #[test]
    fn remove_only_changes_an_active_member() {
        let mut mine = list([member(PC2, "pc2")]);
        assert!(mine.remove(PC2));
        assert!(!mine.remove(PC2));
        assert!(!mine.remove(PC3));
        assert_eq!(mine.len(), 1);
    }

    #[test]
    fn merge_stops_adding_at_the_limit() {
        let id = |i: usize| {
            let mut bytes = [0; 32];
            bytes[..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
            NodeId::from_bytes(bytes)
        };
        let mut mine = Members::default();
        let changes = mine.merge(ME, PC1, list((0..=MAX_MEMBERS).map(|i| member(id(i), "x"))));
        assert_eq!(changes.len(), MAX_MEMBERS);
        assert_eq!(mine.len(), MAX_MEMBERS);
        assert!(mine.get(id(MAX_MEMBERS)).is_none());
        // Known members can still be removed.
        assert_eq!(
            mine.merge(ME, PC1, list([removed(id(0))])),
            [Change::Removed(id(0))]
        );
    }
}
