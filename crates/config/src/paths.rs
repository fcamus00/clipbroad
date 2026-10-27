use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::Error;

/// Default locations of clipbroad's files on the current OS.
///
/// | OS | Config file | Secret key file |
/// |---|---|---|
/// | Linux | `$XDG_CONFIG_HOME/clipbroad/config.toml` (`~/.config/...`) | `$XDG_DATA_HOME/clipbroad/secret.key` (`~/.local/share/...`) |
/// | macOS | `~/Library/Application Support/clipbroad/config.toml` | `~/Library/Application Support/clipbroad/secret.key` |
/// | Windows | `%LOCALAPPDATA%\clipbroad\config.toml` | `%LOCALAPPDATA%\clipbroad\secret.key` |
///
/// On Windows the *local* (non-roaming) directory is used on purpose: the peer list and, above all,
/// the identity belong to one machine and must not follow a roaming profile to another one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub secret_key_file: PathBuf,
}

impl Paths {
    /// Resolves the default paths from the OS conventions and the current environment.
    ///
    /// # Errors
    ///
    /// [`Error::NoHomeDir`] if the home directory cannot be determined.
    pub fn discover() -> Result<Self, Error> {
        let home = env::home_dir().ok_or(Error::NoHomeDir)?;
        Ok(Self::resolve(&home, |var| env::var_os(var)))
    }

    /// Paths for a config file given by the user: the identity is `secret.key` in the same
    /// directory, so each config file has its own identity.
    #[must_use]
    pub fn beside_config(config_file: PathBuf) -> Self {
        Self {
            secret_key_file: config_file.with_file_name("secret.key"),
            config_file,
        }
    }

    /// Builds the paths from a home directory and an environment lookup, so tests can fake both.
    fn resolve(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        // Relative values are invalid per the XDG spec and unsafe on Windows: ignore them.
        let dir_var = |var: &str, fallback: &[&str]| {
            env(var)
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
                .unwrap_or_else(|| fallback.iter().fold(home.to_owned(), |dir, p| dir.join(p)))
                .join("clipbroad")
        };

        let (config_dir, key_dir) = if cfg!(windows) {
            let dir = dir_var("LOCALAPPDATA", &["AppData", "Local"]);
            (dir.clone(), dir)
        } else if cfg!(target_os = "macos") {
            let dir = home.join("Library/Application Support/clipbroad");
            (dir.clone(), dir)
        } else {
            (
                dir_var("XDG_CONFIG_HOME", &[".config"]),
                dir_var("XDG_DATA_HOME", &[".local", "share"]),
            )
        };

        Self {
            config_file: config_dir.join("config.toml"),
            secret_key_file: key_dir.join("secret.key"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\me")
        } else {
            PathBuf::from("/home/me")
        }
    }

    #[test]
    fn discover_finds_two_separate_files() {
        // CI runners always have a home directory.
        let paths = Paths::discover().expect("home directory is known");
        assert!(paths.config_file.ends_with("clipbroad/config.toml"));
        assert!(paths.secret_key_file.ends_with("clipbroad/secret.key"));
        assert!(paths.config_file.is_absolute());
    }

    #[test]
    fn defaults_without_environment() {
        let paths = Paths::resolve(&home(), |_| None);
        let (config, key) = if cfg!(windows) {
            (
                r"C:\Users\me\AppData\Local\clipbroad\config.toml",
                r"C:\Users\me\AppData\Local\clipbroad\secret.key",
            )
        } else if cfg!(target_os = "macos") {
            (
                "/home/me/Library/Application Support/clipbroad/config.toml",
                "/home/me/Library/Application Support/clipbroad/secret.key",
            )
        } else {
            (
                "/home/me/.config/clipbroad/config.toml",
                "/home/me/.local/share/clipbroad/secret.key",
            )
        };
        assert_eq!(paths.config_file, PathBuf::from(config));
        assert_eq!(paths.secret_key_file, PathBuf::from(key));
    }

    #[test]
    fn environment_overrides_defaults() {
        let (vars, root): (&[&str], &str) = if cfg!(windows) {
            (&["LOCALAPPDATA"], r"D:\local")
        } else {
            (&["XDG_CONFIG_HOME", "XDG_DATA_HOME"], "/custom")
        };
        let paths = Paths::resolve(&home(), |var| {
            vars.contains(&var).then(|| OsString::from(root))
        });
        if cfg!(target_os = "macos") {
            // macOS has no such variables.
            assert!(paths.config_file.starts_with("/home/me"));
        } else {
            assert!(paths.config_file.starts_with(root));
            assert!(paths.secret_key_file.starts_with(root));
        }
    }

    #[test]
    fn relative_environment_values_are_ignored() {
        let paths = Paths::resolve(&home(), |_| Some(OsString::from("relative")));
        assert!(paths.config_file.starts_with(home()));
        assert!(paths.secret_key_file.starts_with(home()));
    }
}
