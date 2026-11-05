//! The members of the mesh (see `clipbroad_core::members`): kept in `members.toml`, shared with
//! the peers, and turned into the node's peers.
//!
//! A machine joins by pairing with any member. The members tell each other about it, so it ends
//! up syncing with all of them. `clipbroad remove` takes a machine out of every member the same
//! way, for good.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, bail};
use clipbroad_config::{Config, Paths, Peer, members as file};
use clipbroad_core::members::{Change, Member, Members};
use clipbroad_core::protocol::NodeId;
use clipbroad_net::{Node, ReceivedMembers};
use iroh::EndpointId;
use tracing::{info, warn};

/// The members this machine knows. Every change is saved before it reaches the node.
#[derive(Debug)]
pub struct Mesh {
    file: PathBuf,
    me: NodeId,
    members: Mutex<Members>,
}

impl Mesh {
    /// Loads the members file and adds the peers written in `config`, saving the file if that
    /// changed it. `me` is this machine's id.
    pub fn load(paths: &Paths, config: &Config, me: EndpointId) -> Result<Self> {
        let path = paths.members_file();
        let mut members = file::load(&path)?;
        let mut changed = false;
        for peer in &config.peers {
            changed |= members.configure(Member {
                id: node_id(peer.id),
                name: peer.name.clone(),
                addrs: peer.addrs.clone(),
                added_by: None,
                removed: false,
            });
        }
        if changed {
            file::save(&path, &members)?;
        }
        Ok(Self {
            file: path,
            me: node_id(me),
            members: Mutex::new(members),
        })
    }

    /// The members to sync with.
    pub fn peers(&self) -> Vec<Peer> {
        self.lock().active().filter_map(to_peer).collect()
    }

    /// Who added `id`, if another machine did: `None` for this machine and for peers from the
    /// config file.
    pub fn added_by(&self, id: EndpointId) -> Option<EndpointId> {
        let added_by = self.lock().get(node_id(id))?.added_by?;
        (added_by != self.me)
            .then(|| endpoint_id(added_by))
            .flatten()
    }

    /// Saves a machine that was just paired with this one, and starts syncing with it on `node`
    /// and telling the other members about it. Returns `false` if it was a member already.
    pub fn admit(&self, peer: &Peer, node: Option<&Node>) -> Result<bool> {
        let member = Member {
            id: node_id(peer.id),
            name: peer.name.clone(),
            addrs: peer.addrs.clone(),
            added_by: Some(self.me),
            removed: false,
        };
        let added = self.change(|members| members.admit(member).map_err(anyhow::Error::from))?;
        if let Some(node) = node {
            node.add_peer(peer);
            if added {
                self.announce(node);
            }
        }
        Ok(added)
    }

    /// Merges the list a peer sent: syncs with the members it brings and stops syncing with
    /// the ones it removed. Passes the change on to the other peers.
    pub fn merge(&self, received: ReceivedMembers, node: &Node) {
        let from = node_id(received.from);
        let mut members = self.lock();
        let changes = members.merge(self.me, from, received.members);
        if changes.is_empty() {
            return;
        }
        // The list is in memory already: a failed save only loses it on a restart.
        if let Err(error) = file::save(&self.file, &members) {
            warn!("cannot save the members of the mesh: {error:#}");
        }
        let from = received.from.fmt_short();
        for change in changes {
            match change {
                Change::Added(id) => {
                    let Some(peer) = members.get(id).and_then(to_peer) else {
                        continue;
                    };
                    info!(peer = %peer.id.fmt_short(), name = ?peer.name, %from, "new member");
                    node.add_peer(&peer);
                }
                Change::Removed(id) => {
                    let Some(id) = endpoint_id(id) else { continue };
                    if node.remove_peer(id) {
                        info!(peer = %id.fmt_short(), %from, "member removed");
                    }
                }
            }
        }
        announce(&members, node);
    }

    /// Removes the member whose name or id is `query` for good, and stops syncing with it on
    /// `node`, telling the other members. An id can be shortened to its first characters.
    pub fn remove(&self, query: &str, node: Option<&Node>) -> Result<Member> {
        let removed = self.change(|members| {
            let member = find(members, query)?;
            members.remove(member.id);
            Ok(member)
        })?;
        if let Some(node) = node {
            if let Some(id) = endpoint_id(removed.id) {
                node.remove_peer(id);
            }
            self.announce(node);
        }
        Ok(removed)
    }

    /// Tells the node's peers the current list.
    pub fn announce(&self, node: &Node) {
        announce(&self.lock(), node);
    }

    /// Applies `change` to a copy of the members and saves it. Only then the copy replaces the
    /// list, so a failed save changes nothing.
    fn change<T>(&self, change: impl FnOnce(&mut Members) -> Result<T>) -> Result<T> {
        let mut members = self.lock();
        let mut next = members.clone();
        let result = change(&mut next)?;
        if next != *members {
            file::save(&self.file, &next).context("cannot save the members of the mesh")?;
            *members = next;
        }
        Ok(result)
    }

    /// Nothing panics while holding the lock, and every change is a single assignment.
    fn lock(&self) -> MutexGuard<'_, Members> {
        self.members.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn announce(members: &Members, node: &Node) {
    if let Err(error) = node.announce(members) {
        warn!("cannot send the members of the mesh: {error}");
    }
}

/// The one active member named `query`, or whose id starts with it.
fn find(members: &Members, query: &str) -> Result<Member> {
    let query = query.trim();
    let named = |member: &&Member| {
        member
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(query))
    };
    let prefix = query.to_ascii_lowercase();
    let id_starts =
        |member: &&Member| !prefix.is_empty() && member.id.to_string().starts_with(&prefix);
    let mut found: Vec<&Member> = members.active().filter(named).collect();
    if found.is_empty() {
        found = members.active().filter(id_starts).collect();
    }
    match found.as_slice() {
        [member] => Ok((*member).clone()),
        [] => {
            let known: Vec<String> = members.active().map(label).collect();
            if known.is_empty() {
                bail!("no machine named {query:?}: this machine has no peers");
            }
            bail!(
                "no machine named {query:?}; the members are: {}",
                known.join(", ")
            )
        }
        several => {
            let several: Vec<String> = several.iter().map(|member| label(member)).collect();
            bail!(
                "{query:?} matches more than one machine: {}; use more of its id",
                several.join(", ")
            )
        }
    }
}

/// `laptop (ae58ff8833)`.
pub fn label(member: &Member) -> String {
    let mut id = member.id.to_string();
    id.truncate(10);
    match &member.name {
        Some(name) => format!("{name} ({id})"),
        None => id,
    }
}

fn to_peer(member: &Member) -> Option<Peer> {
    Some(Peer {
        id: endpoint_id(member.id)?,
        name: member.name.clone(),
        addrs: member.addrs.clone(),
    })
}

fn node_id(id: EndpointId) -> NodeId {
    NodeId::from_bytes(*id.as_bytes())
}

/// `None` if the bytes are not a valid key, which only a hand-edited file could hold: the node
/// drops such ids from what peers send.
fn endpoint_id(id: NodeId) -> Option<EndpointId> {
    EndpointId::from_bytes(id.as_bytes()).ok()
}

#[cfg(test)]
mod tests;
