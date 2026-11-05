//! `members.toml`: the members of the mesh this node knows, as [`Members`].
//!
//! Only clipbroad writes it, after a pairing, a removal or a change learned from a peer. Each
//! write replaces the whole file atomically (written aside, then renamed), so a crash never
//! leaves half a file. Peers written by hand go in `config.toml` instead.

use std::{fs, io, io::Write as _, path::Path};

use clipbroad_core::members::Members;

use crate::Error;

const HEADER: &str = "# The machines in this clipbroad mesh, kept up to date by clipbroad.\n\
                      # Do not edit: add peers by hand in config.toml, remove them with \
                      `clipbroad remove`.\n\n";

/// Reads the members in `path`, or an empty list if the file does not exist.
///
/// # Errors
///
/// [`Error::Io`] if the file exists but cannot be read, [`Error::Members`] if it is invalid.
pub fn load(path: &Path) -> Result<Members, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Members::default()),
        Err(e) => return Err(Error::io(path, e)),
    };
    toml::from_str(&text).map_err(|source| Error::Members {
        path: path.to_owned(),
        source,
    })
}

/// Replaces the file at `path` with `members`, creating its directory if needed.
///
/// # Errors
///
/// [`Error::Io`] if the file cannot be written.
///
/// # Panics
///
/// Only on a bug: if `members` cannot be written as TOML.
pub fn save(path: &Path, members: &Members) -> Result<(), Error> {
    let body = toml::to_string(members).expect("members are always valid TOML");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| Error::io(dir, e))?;
    file.write_all(HEADER.as_bytes())
        .and_then(|()| file.write_all(body.as_bytes()))
        .map_err(|e| Error::io(file.path(), e))?;
    file.persist(path).map_err(|e| Error::io(path, e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clipbroad_core::{members::Member, protocol::NodeId};

    use super::*;

    const ID_A: &str = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";

    fn members() -> Members {
        let mut members = Members::default();
        let laptop = Member {
            id: ID_A.parse().expect("valid id"),
            name: Some("laptop \"work\"".to_owned()),
            addrs: vec!["192.168.1.20:42420".parse().expect("valid")],
            added_by: Some(NodeId::from_bytes([7; 32])),
            removed: false,
        };
        let gone = Member {
            id: NodeId::from_bytes([9; 32]),
            name: None,
            addrs: Vec::new(),
            added_by: None,
            removed: false,
        };
        assert_eq!(members.admit(laptop), Ok(true));
        assert_eq!(members.admit(gone.clone()), Ok(true));
        assert!(members.remove(gone.id));
        members
    }

    #[test]
    fn missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let loaded = load(&dir.path().join("members.toml")).expect("no file is fine");
        assert!(loaded.is_empty());
    }

    #[test]
    fn saved_members_load_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("clipbroad").join("members.toml");
        save(&path, &members()).expect("saved");
        assert_eq!(load(&path).expect("valid"), members());

        let text = fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("# The machines"), "{text}");
        // Ids are hex, like in config.toml.
        assert!(text.contains(&format!("id = \"{ID_A}\"")), "{text}");

        // Saving again replaces the file.
        save(&path, &Members::default()).expect("saved");
        assert!(load(&path).expect("valid").is_empty());
    }

    #[test]
    fn invalid_file_names_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("members.toml");
        fs::write(&path, "[[members]]\nid = \"short\"\n").expect("write");
        let err = load(&path).expect_err("bad id");
        assert!(matches!(err, Error::Members { .. }), "{err}");
        assert!(err.to_string().contains("members.toml"), "{err}");
    }
}
