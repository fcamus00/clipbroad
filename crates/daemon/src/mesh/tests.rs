use std::fs;

use iroh::SecretKey;

use super::*;

fn id(seed: u8) -> EndpointId {
    SecretKey::from_bytes(&[seed; 32]).public()
}

fn peer(seed: u8, name: &str) -> Peer {
    Peer {
        id: id(seed),
        name: Some(name.to_owned()),
        addrs: vec!["192.168.1.20:42420".parse().expect("valid")],
    }
}

struct Machine {
    _dir: tempfile::TempDir,
    paths: Paths,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::beside_config(dir.path().join("config.toml"));
        Self { _dir: dir, paths }
    }

    fn mesh(&self, config: &Config) -> Mesh {
        Mesh::load(&self.paths, config, id(0)).expect("loads")
    }

    fn names(mesh: &Mesh) -> Vec<String> {
        mesh.peers()
            .into_iter()
            .filter_map(|peer| peer.name)
            .collect()
    }
}

#[test]
fn config_peers_become_members_and_follow_edits() {
    let machine = Machine::new();
    let mut config = Config {
        peers: vec![peer(1, "desk")],
        ..Config::default()
    };
    let mesh = machine.mesh(&config);
    assert_eq!(mesh.peers(), [peer(1, "desk")]);
    assert_eq!(mesh.added_by(id(1)), None);
    assert!(machine.paths.members_file().exists());

    config.peers[0].addrs.clear();
    let mesh = machine.mesh(&config);
    assert_eq!(mesh.peers()[0].addrs, []);
}

#[test]
fn admitted_machines_are_saved() {
    let machine = Machine::new();
    let mesh = machine.mesh(&Config::default());
    assert!(mesh.admit(&peer(1, "laptop"), None).expect("saved"));
    assert!(!mesh.admit(&peer(1, "laptop"), None).expect("no error"));

    let again = machine.mesh(&Config::default());
    assert_eq!(again.peers(), [peer(1, "laptop")]);
    // Added by this machine, which `added_by` does not report.
    assert_eq!(again.added_by(id(1)), None);
}

#[test]
fn removal_is_saved_and_permanent() {
    let machine = Machine::new();
    let config = Config {
        peers: vec![peer(1, "desk")],
        ..Config::default()
    };
    let mesh = machine.mesh(&config);
    assert!(mesh.admit(&peer(2, "laptop"), None).expect("saved"));

    let removed = mesh.remove("LAPTOP", None).expect("found by name");
    assert_eq!(removed.name.as_deref(), Some("laptop"));
    assert_eq!(Machine::names(&mesh), ["desk"]);
    let err = mesh.admit(&peer(2, "laptop"), None).expect_err("removed");
    assert!(err.to_string().contains("new identity"), "{err:#}");

    // Also a peer from the config file, which stays out after a restart.
    mesh.remove("desk", None).expect("found by name");
    let again = machine.mesh(&config);
    assert_eq!(again.peers(), []);
}

#[test]
fn remove_by_id_takes_an_active_member_only() {
    let machine = Machine::new();
    let mesh = machine.mesh(&Config::default());
    mesh.admit(&peer(1, "laptop"), None).expect("saved");
    assert_eq!(
        mesh.remove_id(id(1), None).expect("removed").id,
        node_id(id(1))
    );
    assert!(mesh.remove_id(id(1), None).is_err(), "already removed");
    assert!(mesh.remove_id(id(2), None).is_err(), "never a member");
}

#[test]
fn remove_finds_one_machine_by_name_or_id_prefix() {
    let machine = Machine::new();
    let mesh = machine.mesh(&Config::default());
    for (seed, name) in [(1, "laptop"), (2, "desk"), (3, "desk")] {
        mesh.admit(&peer(seed, name), None).expect("saved");
    }

    let err = mesh.remove("nope", None).expect_err("unknown");
    assert!(err.to_string().contains("laptop ("), "{err:#}");
    let err = mesh.remove("desk", None).expect_err("two of them");
    assert!(err.to_string().contains("more than one"), "{err:#}");
    assert!(mesh.remove("", None).is_err());

    let prefix = &id(3).to_string()[..12];
    let removed = mesh.remove(prefix, None).expect("found by id");
    assert_eq!(removed.id, node_id(id(3)));
    assert_eq!(Machine::names(&mesh), ["laptop", "desk"]);
    // The file was not touched by the failed attempts.
    let text = fs::read_to_string(machine.paths.members_file()).expect("saved");
    assert_eq!(text.matches("removed = true").count(), 1, "{text}");
}
