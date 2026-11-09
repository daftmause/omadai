//! Non-secret configuration in `$XDG_CONFIG_HOME/omadai/config.toml`.

use crate::error::Error;
use crate::fsutil;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Set once the person stored a credential in an unencrypted keyring with the explicit flag.
    #[serde(default)]
    pub allow_unencrypted_keyring: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xai: Option<XaiConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<GeminiConfig>,
    #[serde(default, rename = "key", skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEntry {
    pub platform: String,
    pub label: String,
    /// The credential's balance-history identity: random, kept across renames, never reused.
    #[serde(default)]
    pub series: String,
    /// Set where `Config::load` minted the series because the file holds none, until it is saved.
    #[serde(skip)]
    pub minted: bool,
}

impl KeyEntry {
    /// A new entry with a fresh series ID, as `key add` writes it.
    pub fn new(platform: &str, label: &str) -> Self {
        KeyEntry {
            platform: platform.to_string(),
            label: label.to_string(),
            series: new_series(),
            minted: false,
        }
    }
}

/// Two outputs of the standard library's OS-seeded hasher, as 32 hex digits: unique enough for a
/// non-secret ID, though not a cryptographic random value.
pub fn new_series() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XaiConfig {
    pub team_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeminiConfig {
    pub project: String,
    pub dataset: String,
    pub table: String,
    /// The dataset's BigQuery location, which every query names; `key add` reads it from `datasets.get`.
    #[serde(default = "default_location")]
    pub location: String,
}

fn default_location() -> String {
    "US".into()
}

impl Config {
    /// Loads the config, minting a series for a hand-written entry without one; `fetch::run` saves
    /// it with `persist_minted` before recording under it.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let mut config = Config::parse(path)?;
        for k in config.keys.iter_mut().filter(|k| k.series.is_empty()) {
            k.series = new_series();
            k.minted = true;
        }
        Ok(config)
    }

    fn parse(path: &Path) -> Result<Self, Error> {
        match fsutil::read_optional(path)? {
            None => Ok(Config::default()),
            Some(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| Error::invalid(format!("{} is not UTF-8", path.display())))?;
                let config: Config = toml::from_str(text)
                    .map_err(|e| Error::invalid(format!("{}: {}", path.display(), e.message())))?;
                // Two entries would share one series once minted, so their spend would be merged.
                for (i, k) in config.keys.iter().enumerate() {
                    if config.keys[..i]
                        .iter()
                        .any(|o| o.platform == k.platform && o.label == k.label)
                    {
                        return Err(Error::invalid(format!(
                            "{}: two [[key]] entries have platform {:?} and label {:?}; remove one from config.toml",
                            path.display(),
                            k.platform,
                            k.label
                        )));
                    }
                }
                Ok(config)
            }
        }
    }

    /// Writes the series `load` minted into the file at `path` for each entry there still without
    /// one, and returns this config with each minted series replaced by the one the file now holds.
    ///
    /// The caller holds the keys lock, so no key command rewrites the file in between.
    pub fn persist_minted(&self, path: &Path) -> Result<Config, Error> {
        let mut on_disk = Config::parse(path)?;
        let mut changed = false;
        for k in self.keys.iter().filter(|k| k.minted) {
            if let Some(d) = on_disk
                .keys
                .iter_mut()
                .find(|d| d.platform == k.platform && d.label == k.label && d.series.is_empty())
            {
                d.series = k.series.clone();
                changed = true;
            }
        }
        if changed {
            on_disk.save(path)?;
        }
        let mut out = self.clone();
        for k in out.keys.iter_mut().filter(|k| k.minted) {
            if let Some(d) = on_disk
                .keys
                .iter()
                .find(|d| d.platform == k.platform && d.label == k.label && !d.series.is_empty())
            {
                k.series = d.series.clone();
                k.minted = false;
            }
        }
        Ok(out)
    }

    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let text =
            toml::to_string(self).map_err(|e| Error::invalid(format!("serialize config: {e}")))?;
        fsutil::atomic_write(path, text.as_bytes())
    }

    pub fn entries(&self, platform: &str) -> impl Iterator<Item = &KeyEntry> {
        self.keys.iter().filter(move |k| k.platform == platform)
    }

    /// The configured labels as a JSON array of `{platform, label}`.
    pub fn keys_json(&self) -> String {
        #[derive(Serialize)]
        struct Label<'a> {
            platform: &'a str,
            label: &'a str,
        }
        let labels: Vec<_> = self
            .keys
            .iter()
            .map(|k| Label {
                platform: &k.platform,
                label: &k.label,
            })
            .collect();
        serde_json::to_string(&labels).expect("labels serialize")
    }

    pub fn has_key(&self, platform: &str, label: &str) -> bool {
        self.keys
            .iter()
            .any(|k| k.platform == platform && k.label == label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TempDir;

    #[test]
    fn round_trip_and_modes() {
        let t = TempDir::new("config");
        let path = t.path().join("omadai").join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let cfg = Config {
            allow_unencrypted_keyring: true,
            xai: Some(XaiConfig {
                team_id: "team".into(),
            }),
            gemini: Some(GeminiConfig {
                project: "p".into(),
                dataset: "d".into(),
                table: "t".into(),
                location: "EU".into(),
            }),
            keys: vec![KeyEntry::new("openai", "work")],
        };
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        assert_eq!(fsutil::mode_of(&path), Some(0o600));
        assert_eq!(fsutil::mode_of(path.parent().unwrap()), Some(0o700));
        assert_eq!(
            cfg.entries("openai").map(|k| &k.label).collect::<Vec<_>>(),
            ["work"]
        );
        assert_eq!(cfg.keys_json(), r#"[{"platform":"openai","label":"work"}]"#);
    }

    #[test]
    fn series_ids_are_assigned_once_and_kept() {
        let t = TempDir::new("config-series");
        let path = t.path().join("config.toml");
        std::fs::write(&path, "[[key]]\nplatform = \"flux\"\nlabel = \"render\"\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        let series = loaded.keys[0].series.clone();
        assert_eq!(series.len(), 32, "{series}");
        assert!(
            series
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "{series}"
        );
        assert!(loaded.keys[0].minted);
        loaded.save(&path).unwrap();
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.keys[0].series, series);
        assert!(!saved.keys[0].minted);
        let a = KeyEntry::new("flux", "render");
        let b = KeyEntry::new("flux", "render");
        assert_ne!(a.series, b.series);
        assert_ne!(a.series, series);
    }

    #[test]
    fn persisting_a_minted_series_keeps_one_the_file_gained_since() {
        let t = TempDir::new("config-persist");
        let path = t.path().join("config.toml");
        let hand_written = "[[key]]\nplatform = \"flux\"\nlabel = \"render\"\n";
        std::fs::write(&path, hand_written).unwrap();
        let first = Config::load(&path).unwrap();
        let mine = first.persist_minted(&path).unwrap();
        assert_eq!(mine.keys[0].series, first.keys[0].series);
        assert!(!mine.keys[0].minted);
        assert_eq!(
            Config::load(&path).unwrap().keys[0].series,
            mine.keys[0].series
        );
        // Another load minted before the file was saved: it takes the saved series, not its own.
        std::fs::write(&path, hand_written).unwrap();
        let late = Config::load(&path).unwrap();
        let saved = Config::load(&path).unwrap().persist_minted(&path).unwrap();
        let resolved = late.persist_minted(&path).unwrap();
        assert_ne!(late.keys[0].series, saved.keys[0].series);
        assert_eq!(resolved.keys[0].series, saved.keys[0].series);
        assert_eq!(
            Config::load(&path).unwrap().keys[0].series,
            saved.keys[0].series
        );
    }

    #[test]
    fn two_entries_for_one_platform_and_label_are_refused() {
        let t = TempDir::new("config-dup");
        let path = t.path().join("config.toml");
        let entry = "[[key]]\nplatform = \"flux\"\nlabel = \"render\"\n";
        std::fs::write(&path, format!("{entry}{entry}")).unwrap();
        let e = Config::load(&path).unwrap_err().to_string();
        assert!(
            e.contains("\"flux\"") && e.contains("\"render\"") && e.contains("remove one"),
            "{e}"
        );
        // The same label on another platform is a different key.
        std::fs::write(
            &path,
            format!("{entry}[[key]]\nplatform = \"fish\"\nlabel = \"render\"\n"),
        )
        .unwrap();
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn a_gemini_export_without_a_location_reads_as_us() {
        let t = TempDir::new("config-location");
        let path = t.path().join("config.toml");
        std::fs::write(
            &path,
            "[gemini]\nproject = \"p\"\ndataset = \"d\"\ntable = \"t\"\n",
        )
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().gemini.unwrap().location, "US");
    }

    #[test]
    fn malformed_config_is_an_error() {
        let t = TempDir::new("config-bad");
        let path = t.path().join("config.toml");
        std::fs::write(&path, "allow_unencrypted_keyring = \"yes\"").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
