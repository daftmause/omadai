//! The per-platform, per-month spend cache and the Flux and Fish Audio balance histories.

use crate::date::{Date, Month};
use crate::error::Error;
use crate::fsutil;
use crate::platform;
use crate::scrub::Scrubbed;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a figure was obtained, ordered from most to least authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FigureKind {
    Billed,
    Estimated,
    Derived,
}

/// What the IDs in a month's breakdown name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Breakdown {
    Key,
    Model,
    Project,
}

pub const CACHE_VERSION: u32 = 1;

/// One platform's month. Amounts are decimals, serialized as strings so they round-trip exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthCache {
    pub version: u32,
    pub platform: String,
    pub month: Month,
    pub kind: FigureKind,
    pub breakdown: Breakdown,
    pub currency: String,
    /// Spend per day, then per key ID (or model or project).
    pub daily: BTreeMap<Date, BTreeMap<String, Decimal>>,
    /// Display names by ID, where the platform returns them.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub credits_remaining: Option<Decimal>,
    /// The settled total for the whole month where `daily` holds only per-key estimates (xAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billed_total: Option<Decimal>,
    #[serde(default)]
    pub data_as_of: Option<String>,
    /// When a fetch last succeeded.
    #[serde(default)]
    pub last_fetched: Option<String>,
    #[serde(default)]
    pub last_error: Option<Scrubbed>,
    /// When a fetch last failed or found a requirement, so a report can tell which attempt is newest.
    #[serde(default)]
    pub last_error_at: Option<String>,
    /// What the last fetch needs (a permission, an account type, a setting), as opposed to a fault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<Scrubbed>,
    /// What a successful fetch left out, such as a retired series whose readings could not be read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Scrubbed>,
    /// Why a credits-left figure could not be computed, kept apart since no spend is missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credits_warnings: Vec<Scrubbed>,
    /// The first day the platform holds data, where that falls after this month's first day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_from: Option<Date>,
}

impl MonthCache {
    pub fn new(platform: &str, month: Month, kind: FigureKind, breakdown: Breakdown) -> Self {
        MonthCache {
            version: CACHE_VERSION,
            platform: platform.to_string(),
            month,
            kind,
            breakdown,
            currency: "USD".into(),
            daily: BTreeMap::new(),
            names: BTreeMap::new(),
            credits_remaining: None,
            billed_total: None,
            data_as_of: None,
            last_fetched: None,
            last_error: None,
            last_error_at: None,
            requirement: None,
            warnings: Vec::new(),
            credits_warnings: Vec::new(),
            history_from: None,
        }
    }

    pub fn add(&mut self, day: Date, id: &str, amount: Decimal) {
        *self
            .daily
            .entry(day)
            .or_default()
            .entry(id.to_string())
            .or_default() += amount;
    }
}

/// Balance readings over time, from which derived spend is computed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceHistory {
    pub version: u32,
    pub platform: String,
    #[serde(deserialize_with = "points")]
    pub points: Vec<BalancePoint>,
    /// The label each series was last recorded under, so a removed key's spend keeps its name.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// Reads points as written now or as an earlier version wrote them, keyed by `label`; a point whose
/// label is null or absent names no series, so it is dropped.
fn points<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<BalancePoint>, D::Error> {
    #[derive(Deserialize)]
    struct Stored {
        #[serde(default, alias = "label")]
        series: Option<String>,
        at: String,
        balance: Decimal,
        #[serde(default)]
        cumulative_top_up: Option<Decimal>,
    }
    let stored = Vec::<Stored>::deserialize(d)?;
    Ok(stored
        .into_iter()
        .filter_map(|p| {
            Some(BalancePoint {
                series: p.series?,
                at: p.at,
                balance: p.balance,
                cumulative_top_up: p.cumulative_top_up,
            })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BalancePoint {
    /// The series the reading belongs to: a credential's series ID, or a Fish Audio wallet.
    pub series: String,
    pub at: String,
    pub balance: Decimal,
    /// Lifetime top-ups where the platform reports them (Fish Audio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_top_up: Option<Decimal>,
}

/// The cache directory: `cache/<platform>/<YYYY-MM>.json` and `balance/<platform>.json`.
#[derive(Debug, Clone)]
pub struct Store {
    state_dir: PathBuf,
}

fn checked(platform_id: &str) -> Result<&str, Error> {
    platform::find(platform_id)
        .map(|p| p.id)
        .ok_or_else(|| Error::invalid(format!("unknown platform {platform_id}")))
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Store {
            state_dir: state_dir.to_path_buf(),
        }
    }

    fn month_path(&self, platform: &str, month: Month) -> Result<PathBuf, Error> {
        Ok(self
            .state_dir
            .join("cache")
            .join(checked(platform)?)
            .join(format!("{month}.json")))
    }

    fn balance_path(&self, platform: &str) -> Result<PathBuf, Error> {
        Ok(self
            .state_dir
            .join("balance")
            .join(format!("{}.json", checked(platform)?)))
    }

    pub fn load_month(&self, platform: &str, month: Month) -> Result<Option<MonthCache>, Error> {
        load(&self.month_path(platform, month)?)
    }

    pub fn save_month(&self, cache: &MonthCache) -> Result<(), Error> {
        save(&self.month_path(&cache.platform, cache.month)?, cache)
    }

    pub fn load_balance(&self, platform: &str) -> Result<Option<BalanceHistory>, Error> {
        load(&self.balance_path(platform)?)
    }

    pub fn save_balance(&self, history: &BalanceHistory) -> Result<(), Error> {
        save(&self.balance_path(&history.platform)?, history)
    }

    /// Platforms that have any cached month, in platform-table order.
    pub fn platforms(&self) -> Vec<&'static str> {
        let root = self.state_dir.join("cache");
        platform::PLATFORMS
            .iter()
            .map(|p| p.id)
            .filter(|id| root.join(id).is_dir())
            .collect()
    }
}

fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    match fsutil::read_optional(path)? {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::invalid(format!("{}: {e}", path.display()))),
    }
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| Error::invalid(format!("serialize {}: {e}", path.display())))?;
    fsutil::atomic_write(path, &bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fsutil::testdir::TempDir;
    use std::str::FromStr;

    #[test]
    fn month_round_trip_is_exact() {
        let t = TempDir::new("cache");
        let store = Store::new(t.path());
        let month = Month::parse("2026-11").unwrap();
        let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
        let big = Decimal::from_str_exact("12345678901234.56789").unwrap();
        c.add(month.first_day(), "key_a", big);
        let tenth = Decimal::from_str_exact("0.1").unwrap();
        for _ in 0..10 {
            c.add(month.first_day().succ(), "key_b", tenth);
        }
        c.names.insert("key_a".into(), "Work".into());
        c.last_error = Some(crate::scrub::plain("HTTP 500"));
        store.save_month(&c).unwrap();
        let back = store.load_month("openai", month).unwrap().unwrap();
        assert_eq!(back, c);
        assert_eq!(
            back.daily[&month.first_day()]["key_a"].to_string(),
            "12345678901234.56789"
        );
        assert_eq!(
            back.daily[&month.first_day().succ()]["key_b"],
            Decimal::from(1)
        );
        assert_eq!(store.platforms(), vec!["openai"]);
        assert_eq!(
            fsutil::mode_of(&t.path().join("cache/openai/2026-11.json")),
            Some(0o600)
        );
        assert_eq!(fsutil::mode_of(&t.path().join("cache/openai")), Some(0o700));
        assert_eq!(fsutil::mode_of(&t.path().join("cache")), Some(0o700));
        // f64 cannot hold this value: the round trip above would have failed through a float.
        assert_ne!(
            f64::from_str("12345678901234.56789").unwrap().to_string(),
            "12345678901234.56789"
        );
    }

    #[test]
    fn provider_json_numbers_parse_exactly() {
        #[derive(Deserialize)]
        struct Amount {
            #[serde(with = "rust_decimal::serde::arbitrary_precision")]
            value: Decimal,
        }
        let a: Amount = serde_json::from_str(r#"{"value": 12345678901234.56789}"#).unwrap();
        assert_eq!(a.value.to_string(), "12345678901234.56789");
        let mut sum = Decimal::ZERO;
        for _ in 0..10 {
            let a: Amount = serde_json::from_str(r#"{"value": 0.1}"#).unwrap();
            sum += a.value;
        }
        assert_eq!(sum, Decimal::ONE);
        let f: f64 = (0..10).map(|_| 0.1f64).sum();
        assert_ne!(f, 1.0, "the f64 path this guards against");
    }

    /// A balance history as an earlier version wrote it, which keyed each point by its credential label.
    pub const LABELLED: &str = r#"{
  "version": 1,
  "platform": "flux",
  "points": [
    {"label": "render", "at": "2026-11-02T09:00:00Z", "balance": "1000"},
    {"label": "render", "at": "2026-11-03T09:00:00Z", "balance": "700"},
    {"label": null, "at": "2026-11-03T10:00:00Z", "balance": "1"},
    {"at": "2026-11-03T11:00:00Z", "balance": "2"}
  ]
}"#;

    #[test]
    fn balance_points_keyed_by_label_load_as_series() {
        let t = TempDir::new("balance-labelled");
        let store = Store::new(t.path());
        std::fs::create_dir_all(t.path().join("balance")).unwrap();
        std::fs::write(t.path().join("balance/flux.json"), LABELLED).unwrap();
        let h = store.load_balance("flux").unwrap().unwrap();
        let points: Vec<_> = h
            .points
            .iter()
            .map(|p| (p.series.as_str(), p.balance))
            .collect();
        assert_eq!(
            points,
            [
                ("render", Decimal::from(1000)),
                ("render", Decimal::from(700))
            ],
            "a point with no label names no series and is dropped"
        );
        store.save_balance(&h).unwrap();
        let saved = std::fs::read_to_string(t.path().join("balance/flux.json")).unwrap();
        assert!(
            saved.contains("\"series\"") && !saved.contains("\"label\""),
            "{saved}"
        );
        assert_eq!(store.load_balance("flux").unwrap().unwrap(), h);
    }

    #[test]
    fn balance_round_trip_and_bad_platform() {
        let t = TempDir::new("balance");
        let store = Store::new(t.path());
        let h = BalanceHistory {
            version: CACHE_VERSION,
            platform: "fish".into(),
            points: vec![BalancePoint {
                series: "fk-series-0001".into(),
                at: "2026-11-05T00:00:00Z".into(),
                balance: Decimal::new(1050, 2),
                cumulative_top_up: Some(Decimal::new(20, 0)),
            }],
            labels: BTreeMap::new(),
        };
        store.save_balance(&h).unwrap();
        assert_eq!(store.load_balance("fish").unwrap().unwrap(), h);
        assert!(
            store
                .load_month("../../etc", Month::parse("2026-11").unwrap())
                .is_err()
        );
    }
}
