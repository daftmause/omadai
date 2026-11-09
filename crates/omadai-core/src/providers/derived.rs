//! Spend derived from a balance history, for platforms with no usage API (Flux, Fish Audio).

use super::day_of;
use crate::cache::{BalanceHistory, BalancePoint, MonthCache};
use crate::date::{Date, Month};
use crate::provider::ProviderError;
use crate::scrub::plain;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// Appends one reading to `series`, and keeps `label` as the name the series goes by.
pub fn record(
    history: &mut BalanceHistory,
    (series, label): (&str, &str),
    at: String,
    balance: Decimal,
    cumulative_top_up: Option<Decimal>,
) {
    history.labels.insert(series.to_string(), label.to_string());
    history.points.push(BalancePoint {
        series: series.to_string(),
        at,
        balance,
        cumulative_top_up,
    });
}

/// Spend per day of `month` up to `today`, from consecutive readings of `series`.
///
/// `step` gives the spend between a reading and the one before it; that spend is filed on the
/// later reading's day, and the first reading ever taken has no predecessor, so it adds nothing.
pub fn daily(
    history: &BalanceHistory,
    series: &str,
    month: Month,
    today: Date,
    step: impl Fn(&BalancePoint, &BalancePoint) -> Result<Decimal, ProviderError>,
) -> Result<BTreeMap<Date, Decimal>, ProviderError> {
    let mut points: Vec<(Date, &BalancePoint)> = Vec::new();
    for p in history.points.iter().filter(|p| p.series == series) {
        points.push((day_of(&p.at)?, p));
    }
    // Every `at` is written by `now_rfc3339` in one fixed UTC form, so text order is time order.
    points.sort_by(|a, b| a.1.at.cmp(&b.1.at));
    let mut out = BTreeMap::new();
    for pair in points.windows(2) {
        let (prev, (day, cur)) = (pair[0].1, pair[1]);
        if day.month() != month || day > today {
            continue;
        }
        let spent = step(prev, cur)?;
        if !spent.is_zero() {
            *out.entry(day).or_default() += spent;
        }
    }
    Ok(out)
}

/// Adds the spend of every series in `history` to `month`, retired ones included, each series
/// from its own readings only, so no delta ever spans two series.
pub fn add_every_series(
    cache: &mut MonthCache,
    history: &BalanceHistory,
    today: Date,
    step: impl Fn(&BalancePoint, &BalancePoint) -> Result<Decimal, ProviderError>,
) -> Result<(), ProviderError> {
    // Whatever the active credentials named is active; any other series is retired.
    let active: Vec<String> = cache.names.keys().cloned().collect();
    let mut series: Vec<&str> = Vec::new();
    for p in &history.points {
        if !series.contains(&p.series.as_str()) {
            series.push(&p.series);
        }
    }
    for s in series {
        let retired = !active.iter().any(|a| a == s);
        let spend = match daily(history, s, cache.month, today, &step) {
            Ok(spend) => spend,
            // A retired series has no credential to fix it, so it must not fail every later fetch.
            Err(e) if retired => {
                let name = history.labels.get(s).map_or(s, String::as_str);
                cache
                    .warnings
                    .push(plain(format!("left out {name} (removed): {}", e.message)));
                continue;
            }
            Err(e) => return Err(e),
        };
        if retired {
            let name = history.labels.get(s).map_or(s, String::as_str);
            cache
                .names
                .insert(s.to_string(), format!("{name} (removed)"));
        }
        for (day, amount) in spend {
            cache.add(day, s, amount);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::cache::{Breakdown, CACHE_VERSION, FigureKind};

    fn history(points: &[(&str, &str, &str)]) -> BalanceHistory {
        let mut h = BalanceHistory {
            version: CACHE_VERSION,
            platform: "flux".into(),
            points: Vec::new(),
            labels: BTreeMap::new(),
        };
        for (series, at, balance) in points {
            record(&mut h, (series, series), (*at).into(), dec(balance), None);
        }
        h
    }

    fn drops(prev: &BalancePoint, cur: &BalancePoint) -> Result<Decimal, ProviderError> {
        Ok((prev.balance - cur.balance).max(Decimal::ZERO))
    }

    #[test]
    fn readings_are_ordered_and_kept_per_series() {
        // Out of order on disk, and another series in between that must not be read as this one's.
        let h = history(&[
            ("a", "2026-11-03T10:00:00Z", "70"),
            ("a", "2026-11-02T10:00:00Z", "100"),
            ("b", "2026-11-02T11:00:00Z", "5"),
        ]);
        let nov = Month::parse("2026-11").unwrap();
        let a = daily(&h, "a", nov, d("2026-11-05"), drops).unwrap();
        assert_eq!(
            a.into_iter().collect::<Vec<_>>(),
            [(d("2026-11-03"), dec("30"))]
        );
        assert!(
            daily(&h, "b", nov, d("2026-11-05"), drops)
                .unwrap()
                .is_empty()
        );
    }

    fn november(active: &[&str]) -> MonthCache {
        let mut c = MonthCache::new(
            "flux",
            Month::parse("2026-11").unwrap(),
            FigureKind::Derived,
            Breakdown::Key,
        );
        for s in active {
            c.names.insert((*s).into(), (*s).into());
        }
        c
    }

    #[test]
    fn a_retired_series_that_cannot_be_read_is_skipped() {
        let h = history(&[
            ("a", "2026-11-02T10:00:00Z", "100"),
            ("a", "2026-11-03T10:00:00Z", "70"),
            ("b", "yesterday", "1"),
            ("b", "2026-11-02T10:00:00Z", "0"),
        ]);
        let mut c = november(&["a"]);
        add_every_series(&mut c, &h, d("2026-11-05"), drops).unwrap();
        assert_eq!(c.daily[&d("2026-11-03")]["a"], dec("30"));
        assert!(!c.daily.values().any(|ids| ids.contains_key("b")), "{c:?}");
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(
            c.warnings[0].as_str().contains("b (removed)"),
            "{:?}",
            c.warnings
        );
    }

    #[test]
    fn an_active_series_that_cannot_be_read_still_fails() {
        let h = history(&[("a", "yesterday", "1"), ("a", "2026-11-02T10:00:00Z", "0")]);
        let mut c = november(&["a"]);
        assert!(add_every_series(&mut c, &h, d("2026-11-05"), drops).is_err());
    }

    #[test]
    fn bad_timestamp_is_an_error() {
        let h = history(&[("a", "yesterday", "1"), ("a", "2026-11-02T10:00:00Z", "0")]);
        let nov = Month::parse("2026-11").unwrap();
        assert!(daily(&h, "a", nov, d("2026-11-05"), drops).is_err());
    }
}
