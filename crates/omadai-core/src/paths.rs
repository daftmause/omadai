use crate::error::Error;
use std::ffi::OsString;
use std::path::PathBuf;

/// Where omadai reads and writes, resolved from the XDG base-directory variables.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `$HOME`, where only `omadai log export` writes, so the other verbs run without it.
    pub home: Option<PathBuf>,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// gnome-keyring's storage directory, used only to detect an unencrypted default keyring.
    pub keyrings_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self, Error> {
        Self::from_lookup(|k| std::env::var_os(k))
    }

    /// Resolves with `lookup` standing in for the environment, so tests never mutate process state.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, Error> {
        let home = || -> Result<PathBuf, Error> {
            lookup("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .ok_or_else(|| Error::invalid("HOME is not set to an absolute path"))
        };
        // The XDG spec says a relative or empty value is invalid and must be ignored.
        let xdg = |var: &str, fallback: &[&str]| -> Result<PathBuf, Error> {
            match lookup(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
                Some(p) => Ok(p),
                None => Ok(fallback.iter().fold(home()?, |p, part| p.join(part))),
            }
        };
        Ok(Paths {
            home: home().ok(),
            config_dir: xdg("XDG_CONFIG_HOME", &[".config"])?.join("omadai"),
            state_dir: xdg("XDG_STATE_HOME", &[".local", "state"])?.join("omadai"),
            keyrings_dir: xdg("XDG_DATA_HOME", &[".local", "share"])?.join("keyrings"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("lock")
    }

    /// Serializes `key add`, `key rename` and `key remove`, which each rewrite the config.
    pub fn keys_lock_file(&self) -> PathBuf {
        self.state_dir.join("keys.lock")
    }

    /// Serializes every read-modify-write of the error log; `fetch` takes it only under `lock`.
    pub fn log_lock_file(&self) -> PathBuf {
        self.state_dir.join("log.lock")
    }

    pub fn errors_file(&self) -> PathBuf {
        self.state_dir.join("errors.json")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.state_dir.join("cache")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn resolve(vars: &[(&str, &str)]) -> Result<Paths, Error> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        Paths::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn xdg_vars_win() {
        let p = resolve(&[
            ("HOME", "/h"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_DATA_HOME", "/d"),
        ])
        .unwrap();
        assert_eq!(p.config_file(), Path::new("/c/omadai/config.toml"));
        assert_eq!(p.lock_file(), Path::new("/s/omadai/lock"));
        assert_eq!(p.keyrings_dir, Path::new("/d/keyrings"));
        assert_eq!(p.home.as_deref(), Some(Path::new("/h")));
        assert_eq!(p.log_lock_file(), Path::new("/s/omadai/log.lock"));
        assert_eq!(p.errors_file(), Path::new("/s/omadai/errors.json"));
    }

    #[test]
    fn fallbacks_and_relative_ignored() {
        let p = resolve(&[
            ("HOME", "/h"),
            ("XDG_CONFIG_HOME", "rel"),
            ("XDG_STATE_HOME", ""),
        ])
        .unwrap();
        assert_eq!(p.config_dir, Path::new("/h/.config/omadai"));
        assert_eq!(p.state_dir, Path::new("/h/.local/state/omadai"));
        assert_eq!(p.keyrings_dir, Path::new("/h/.local/share/keyrings"));
        assert!(resolve(&[]).is_err());
    }

    #[test]
    fn xdg_vars_alone_need_no_home() {
        let p = resolve(&[
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_DATA_HOME", "/d"),
        ])
        .unwrap();
        assert_eq!(p.home, None);
        assert_eq!(p.state_dir, Path::new("/s/omadai"));
    }
}
