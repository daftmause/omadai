//! Spend over a date range from the cache alone; no network.

use crate::cache::{Breakdown, FigureKind, Store};
use crate::date::{Date, Month};
use crate::error::Error;
use crate::platform;
use crate::scrub::{Scrubbed, printable, printable_lines};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;

/// An inclusive range of days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Range {
    pub from: Date,
    pub to: Date,
}

impl Range {
    pub fn month(m: Month) -> Self {
        Range {
            from: m.first_day(),
            to: m.last_day(),
        }
    }

    pub fn new(from: Date, to: Date) -> Result<Self, Error> {
        if from > to {
            return Err(Error::invalid(format!("--from {from} is after --to {to}")));
        }
        Ok(Range { from, to })
    }

    fn months(self) -> impl Iterator<Item = Month> {
        let last = self.to.month();
        std::iter::successors(Some(self.from.month()), move |m| {
            (*m < last).then(|| m.succ())
        })
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub from: Date,
    pub to: Date,
    pub currency: &'static str,
    pub total: Decimal,
    pub platforms: Vec<PlatformReport>,
}

#[derive(Debug, Serialize)]
pub struct PlatformReport {
    pub platform: &'static str,
    pub name: &'static str,
    /// The least authoritative kind among the months in range.
    pub kind: FigureKind,
    pub breakdown: Breakdown,
    pub total: Decimal,
    pub items: Vec<Item>,
    /// Settled totals minus the per-key estimates, over the whole settled months in range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settlement_adjustment: Option<Decimal>,
    /// From the month in range fetched successfully most recently.
    pub credits_remaining: Option<Decimal>,
    pub data_as_of: Option<String>,
    /// The most recent successful fetch of any month in range.
    pub last_fetched: Option<String>,
    /// The error of the month in range with the newest fetch attempt, if that attempt failed.
    pub last_error: Option<Scrubbed>,
    /// What the platform needs from the person, if the newest fetch attempt in range found that.
    pub requirement: Option<Scrubbed>,
    /// The month `last_error` or `requirement` belongs to.
    pub status_month: Option<Month>,
    /// Months in range with no readable cache, so `total` leaves them out.
    pub missing_months: Vec<Month>,
    /// Cache files in range that could not be read.
    pub warnings: Vec<String>,
    /// Months in range before the platform's history, left out of every figure and status.
    pub before_history: Vec<Month>,
    /// The latest first day of history of any month in range, whole or partial.
    pub history_from: Option<Date>,
}

#[derive(Debug, Serialize)]
pub struct Item {
    pub id: String,
    pub name: Option<String>,
    pub total: Decimal,
}

/// The status a month's newest fetch attempt left, and when that attempt ran.
struct Attempt {
    at: Option<String>,
    month: Month,
    last_error: Option<Scrubbed>,
    requirement: Option<Scrubbed>,
}

pub fn build(store: &Store, range: Range, only: Option<&str>) -> Result<Report, Error> {
    let mut platforms = Vec::new();
    for id in store.platforms() {
        if only.is_some_and(|o| o != id) {
            continue;
        }
        let info = platform::find(id).expect("store lists known platforms");
        let mut kind: Option<FigureKind> = None;
        let mut breakdown = Breakdown::Key;
        let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        let mut adjustment: Option<Decimal> = None;
        let mut last_fetched: Option<String> = None;
        let (mut credits_remaining, mut data_as_of) = (None, None);
        let mut attempted: Option<Attempt> = None;
        let (mut missing_months, mut warnings) = (Vec::new(), Vec::new());
        let mut never_fetched: Vec<(Month, String)> = Vec::new();
        let (mut before_history, mut history_from) = (Vec::new(), None);
        // Whole-before months with their floor, and fetched months with the day their figures start,
        // for the per-month floors.
        let mut wholly_before: Vec<(Month, Date)> = Vec::new();
        let mut with_figures: Vec<(Month, Date)> = Vec::new();
        for month in range.months() {
            let c = match store.load_month(id, month) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    missing_months.push(month);
                    continue;
                }
                Err(e) => {
                    missing_months.push(month);
                    warnings.push(format!("{month}: {UNREADABLE}, left out: {e}"));
                    continue;
                }
            };
            if let Some(first) = c.history_from {
                if first > month.last_day() {
                    before_history.push(month);
                    wholly_before.push((month, first));
                    continue;
                }
                // A partial month's floor falls inside it.
                history_from = history_from.max(Some(first));
            }
            // A fetched month is known from its floor or its first day, even at $0.00.
            if c.last_fetched.is_some() {
                with_figures.push((month, c.history_from.unwrap_or(month.first_day())));
            }
            warnings.extend(c.warnings.iter().map(|w| format!("{month}: {w}")));
            // A placeholder from a failed first fetch holds no figures, so that month is not covered.
            if c.last_fetched.is_none() {
                missing_months.push(month);
                if let Some(e) = c.requirement.as_ref().or(c.last_error.as_ref()) {
                    never_fetched.push((month, e.to_string()));
                }
            }
            if c.last_fetched.is_some() && c.last_fetched >= last_fetched {
                last_fetched.clone_from(&c.last_fetched);
                credits_remaining = c.credits_remaining;
                data_as_of.clone_from(&c.data_as_of);
            }
            // A month's last attempt is its last success or its last failure, whichever is newer;
            // months run oldest first, so a tie goes to the later month.
            let attempt = c.last_fetched.clone().max(c.last_error_at.clone());
            if attempted.as_ref().is_none_or(|a| attempt >= a.at) {
                attempted = Some(Attempt {
                    at: attempt,
                    month,
                    last_error: c.last_error.clone(),
                    requirement: c.requirement.clone(),
                });
            }
            let whole = range.from <= month.first_day() && range.to >= month.last_day();
            let month_kind = match c.billed_total {
                Some(billed) if whole => {
                    let estimated: Decimal = c.daily.values().flat_map(|m| m.values()).sum();
                    *adjustment.get_or_insert(Decimal::ZERO) += billed - estimated;
                    c.kind
                }
                // Part of a settled month is only the per-key estimates.
                Some(_) => c.kind.max(FigureKind::Estimated),
                None => c.kind,
            };
            kind = Some(kind.map_or(month_kind, |k| k.max(month_kind)));
            breakdown = c.breakdown;
            for (_, by_id) in c.daily.range(range.from..=range.to) {
                for (key, amount) in by_id {
                    *totals.entry(key.clone()).or_default() += *amount;
                }
            }
            names.extend(c.names.clone());
        }
        // A whole-before month's gap ends where the next fetched month's figures start.
        for (month, first) in wholly_before {
            let next = with_figures
                .iter()
                .find(|(m, _)| *m > month)
                .map(|(_, start)| *start);
            history_from = history_from.max(Some(next.map_or(first, |n| n.min(first))));
        }
        let Some(kind) = kind else {
            if warnings.is_empty() && before_history.is_empty() {
                continue;
            }
            // Only unreadable files or months before history: still a row, so either is seen.
            platforms.push(PlatformReport {
                platform: info.id,
                name: info.name,
                kind: FigureKind::Estimated,
                breakdown,
                total: Decimal::ZERO,
                items: Vec::new(),
                settlement_adjustment: None,
                credits_remaining: None,
                data_as_of: None,
                last_fetched: None,
                last_error: None,
                requirement: None,
                status_month: None,
                missing_months,
                warnings,
                before_history,
                history_from,
            });
            continue;
        };
        let mut items: Vec<Item> = totals
            .into_iter()
            .map(|(id, total)| Item {
                name: names.get(&id).cloned(),
                id,
                total,
            })
            .collect();
        items.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.id.cmp(&b.id)));
        // Never both: a requirement is what the attempt found, so it outranks a leftover error.
        let (last_error, requirement, status_month) = match attempted {
            Some(a) if a.requirement.is_some() => (None, a.requirement, Some(a.month)),
            Some(a) if a.last_error.is_some() => (a.last_error, None, Some(a.month)),
            _ => (None, None, None),
        };
        // The status month's failure is already shown as its status; any other is kept as a warning.
        for (month, e) in never_fetched {
            if Some(month) != status_month {
                warnings.push(format!("{month}: never fetched: {e}"));
            }
        }
        platforms.push(PlatformReport {
            platform: info.id,
            name: info.name,
            kind,
            breakdown,
            total: items.iter().map(|i| i.total).sum::<Decimal>() + adjustment.unwrap_or_default(),
            items,
            settlement_adjustment: adjustment,
            credits_remaining,
            data_as_of,
            last_fetched,
            last_error,
            requirement,
            status_month,
            missing_months,
            warnings,
            before_history,
            history_from,
        });
    }
    Ok(Report {
        from: range.from,
        to: range.to,
        currency: "USD",
        total: platforms.iter().map(|p| p.total).sum(),
        platforms,
    })
}

pub fn to_json(report: &Report) -> String {
    serde_json::to_string(report).expect("report serializes")
}

fn dollars(d: Decimal) -> String {
    format!(
        "${:.2}",
        d.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
    )
}

fn kind_label(k: FigureKind) -> &'static str {
    match k {
        FigureKind::Billed => "billed",
        FigureKind::Estimated => "estimated",
        FigureKind::Derived => "derived",
    }
}

/// How an unreadable month's warning reads, so `to_text` can name it ahead of other notes.
const UNREADABLE: &str = "unreadable cache";

pub fn to_text(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Spend {} to {}", report.from, report.to);
    if report.platforms.is_empty() {
        let _ = writeln!(out, "No cached figures for this range. Run `omadai fetch`.");
    }
    for p in &report.platforms {
        if p.last_fetched.is_none() {
            let why = match p.history_from {
                _ if p.warnings.iter().any(|w| w.contains(UNREADABLE)) => {
                    "a cached month could not be read".into()
                }
                _ if p.warnings.is_empty() && !p.missing_months.is_empty() => {
                    "not fetched yet".into()
                }
                Some(first) if !p.before_history.is_empty() => format!("no history before {first}"),
                _ if p.warnings.is_empty() => "not fetched yet".into(),
                _ => "no readable figures".into(),
            };
            let _ = writeln!(out, "{:<14} {:>14}  {why}", p.name, "-");
        } else {
            let _ = writeln!(
                out,
                "{:<14} {:>14}  {}",
                p.name,
                dollars(p.total),
                kind_label(p.kind)
            );
            if let Some(first) = p.history_from {
                let _ = writeln!(out, "  history from {first}");
            }
        }
        for i in &p.items {
            let _ = writeln!(
                out,
                "  {:<28} {:>12}",
                printable(i.name.as_deref().unwrap_or(&i.id)),
                dollars(i.total)
            );
        }
        if let Some(a) = p.settlement_adjustment {
            let _ = writeln!(out, "  {:<28} {:>12}", "settlement adjustment", dollars(a));
        }
        if let Some(c) = p.credits_remaining {
            let _ = writeln!(out, "  credits remaining {}", dollars(c));
        }
        let of = p
            .status_month
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        if let Some(e) = &p.last_error {
            let _ = writeln!(out, "  last error{of}: {}", printable_lines(e.as_str()));
        }
        if let Some(n) = &p.requirement {
            let _ = writeln!(out, "  needs{of}: {}", printable_lines(n.as_str()));
        }
        if !p.missing_months.is_empty() {
            let months: Vec<String> = p.missing_months.iter().map(|m| m.to_string()).collect();
            let _ = writeln!(out, "  missing: {} (not in the total)", months.join(", "));
        }
        for w in &p.warnings {
            let _ = writeln!(out, "  warning: {}", printable_lines(w.as_str()));
        }
    }
    let _ = writeln!(out, "{:<14} {:>14}", "Total", dollars(report.total));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MonthCache;
    use crate::fsutil::testdir::TempDir;

    fn d(s: &str) -> Date {
        Date::parse(s).unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str_exact(s).unwrap()
    }

    fn seed(store: &Store) {
        let oct = Month::parse("2026-10").unwrap();
        let nov = Month::parse("2026-11").unwrap();
        let mut a = MonthCache::new("openai", oct, FigureKind::Billed, Breakdown::Key);
        a.add(d("2026-10-30"), "k1", dec("1.10"));
        a.add(d("2026-10-31"), "k2", dec("2.00"));
        a.names.insert("k1".into(), "Work".into());
        store.save_month(&a).unwrap();
        let mut b = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        b.add(d("2026-11-01"), "k1", dec("0.15"));
        b.add(d("2026-11-15"), "k1", dec("10.005"));
        store.save_month(&b).unwrap();
        let mut x = MonthCache::new("xai", nov, FigureKind::Estimated, Breakdown::Key);
        x.add(d("2026-11-02"), "x1", dec("3.3"));
        store.save_month(&x).unwrap();
    }

    #[test]
    fn month_range_platform_and_total() {
        let t = TempDir::new("report");
        let store = Store::new(t.path());
        seed(&store);
        let nov = build(&store, Range::month(Month::parse("2026-11").unwrap()), None).unwrap();
        assert_eq!(nov.platforms.len(), 2);
        assert_eq!(nov.platforms[0].total, dec("10.155"));
        assert_eq!(nov.platforms[1].kind, FigureKind::Estimated);
        assert_eq!(nov.total, dec("13.455"));

        let span = build(
            &store,
            Range::new(d("2026-10-31"), d("2026-11-01")).unwrap(),
            None,
        )
        .unwrap();
        let openai = &span.platforms[0];
        assert_eq!(openai.total, dec("2.15"));
        assert_eq!(openai.items[0].id, "k2");
        assert_eq!(openai.items[1].name.as_deref(), Some("Work"));
        assert_eq!(span.total, dec("2.15"));

        let only = build(
            &store,
            Range::month(Month::parse("2026-11").unwrap()),
            Some("xai"),
        )
        .unwrap();
        assert_eq!(only.platforms.len(), 1);
        assert_eq!(only.total, dec("3.3"));

        assert!(Range::new(d("2026-11-02"), d("2026-11-01")).is_err());
        let empty = build(&store, Range::month(Month::parse("2025-01").unwrap()), None).unwrap();
        assert!(empty.platforms.is_empty());
        assert_eq!(empty.total, Decimal::ZERO);
    }

    #[test]
    fn settled_month_total_is_the_settlement() {
        let t = TempDir::new("report-settled");
        let store = Store::new(t.path());
        let oct = Month::parse("2026-10").unwrap();
        let mut c = MonthCache::new("xai", oct, FigureKind::Billed, Breakdown::Key);
        c.add(d("2026-10-03"), "x1", dec("6.00"));
        c.add(d("2026-10-20"), "x2", dec("4.23"));
        c.billed_total = Some(dec("10.00"));
        store.save_month(&c).unwrap();
        let whole = build(&store, Range::month(oct), None).unwrap();
        let x = &whole.platforms[0];
        assert_eq!(x.total, dec("10.00"));
        assert_eq!(x.kind, FigureKind::Billed);
        assert_eq!(x.settlement_adjustment, Some(dec("-0.23")));
        assert_eq!(x.items[0].total, dec("6.00"), "per-key estimates are kept");
        assert!(to_text(&whole).contains("settlement adjustment"));
        let part = build(
            &store,
            Range::new(d("2026-10-01"), d("2026-10-10")).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(part.platforms[0].total, dec("6.00"));
        assert_eq!(part.platforms[0].kind, FigureKind::Estimated);
        assert_eq!(part.platforms[0].settlement_adjustment, None);
    }

    #[test]
    fn month_warnings_reach_the_report() {
        let t = TempDir::new("report-warn");
        let store = Store::new(t.path());
        let nov = Month::parse("2026-11").unwrap();
        let mut n = MonthCache::new("flux", nov, FigureKind::Derived, Breakdown::Key);
        n.last_fetched = Some("2026-11-05T09:00:00Z".into());
        n.warnings
            .push(crate::scrub::plain("left out b (removed): bad reading"));
        store.save_month(&n).unwrap();
        let r = build(&store, Range::month(nov), None).unwrap();
        assert_eq!(
            r.platforms[0].warnings,
            vec!["2026-11: left out b (removed): bad reading".to_string()]
        );
    }

    #[test]
    fn never_fetched_month_is_missing_after_another_month_succeeds() {
        let t = TempDir::new("report-never");
        let store = Store::new(t.path());
        let oct = Month::parse("2026-10").unwrap();
        let nov = Month::parse("2026-11").unwrap();
        let mut o = MonthCache::new("openai", oct, FigureKind::Estimated, Breakdown::Key);
        o.last_error = Some(crate::scrub::plain("HTTP 500"));
        o.last_error_at = Some("2026-11-01T10:00:00Z".into());
        store.save_month(&o).unwrap();
        let mut n = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        n.add(d("2026-11-02"), "k1", dec("2"));
        n.last_fetched = Some("2026-11-05T09:00:00Z".into());
        store.save_month(&n).unwrap();
        let range = Range::new(d("2026-10-01"), d("2026-11-30")).unwrap();
        let r = build(&store, range, None).unwrap();
        let p = &r.platforms[0];
        assert_eq!(p.missing_months, vec![oct]);
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("2026-10") && w.contains("HTTP 500")),
            "{:?}",
            p.warnings
        );
        assert!(to_text(&r).contains("missing: 2026-10 (not in the total)"));
    }

    #[test]
    fn multi_month_status_spans_the_range_and_names_missing_months() {
        let t = TempDir::new("report-span");
        let store = Store::new(t.path());
        let oct = Month::parse("2026-10").unwrap();
        let nov = Month::parse("2026-11").unwrap();
        let mut o = MonthCache::new("openai", oct, FigureKind::Billed, Breakdown::Key);
        o.add(d("2026-10-05"), "k1", dec("1"));
        o.last_fetched = Some("2026-11-05T10:00:00Z".into());
        o.credits_remaining = Some(dec("4.5"));
        o.data_as_of = Some("2026-11-05T09:00:00Z".into());
        store.save_month(&o).unwrap();
        let mut n = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        n.add(d("2026-11-02"), "k1", dec("2"));
        n.last_fetched = Some("2026-11-04T09:00:00Z".into());
        n.credits_remaining = Some(dec("9"));
        n.data_as_of = Some("2026-11-04T08:00:00Z".into());
        n.last_error = Some(crate::scrub::plain("HTTP 500"));
        n.last_error_at = Some("2026-11-05T12:00:00Z".into());
        store.save_month(&n).unwrap();
        let range = Range::new(d("2026-09-01"), d("2026-11-30")).unwrap();
        let r = build(&store, range, None).unwrap();
        let p = &r.platforms[0];
        assert_eq!(p.missing_months, vec![Month::parse("2026-09").unwrap()]);
        assert_eq!(p.last_error.as_ref().map(|e| e.as_str()), Some("HTTP 500"));
        assert_eq!(p.status_month, Some(nov));
        assert_eq!(p.last_fetched.as_deref(), Some("2026-11-05T10:00:00Z"));
        // The newest success is October's, though November is the later month.
        assert_eq!(p.credits_remaining, Some(dec("4.5")));
        assert_eq!(p.data_as_of.as_deref(), Some("2026-11-05T09:00:00Z"));
        assert_eq!(p.total, dec("3"));
        let json: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(json["platforms"][0]["missing_months"][0], "2026-09");
        assert_eq!(json["platforms"][0]["status_month"], "2026-11");
        let text = to_text(&r);
        assert!(
            text.contains("missing: 2026-09 (not in the total)"),
            "{text}"
        );
        assert!(text.contains("last error (2026-11): HTTP 500"), "{text}");
        assert!(text.contains("credits remaining $4.50"), "{text}");

        // An older month's error does not linger once a newer attempt elsewhere succeeded.
        o.last_fetched = Some("2026-11-01T10:00:00Z".into());
        o.last_error = Some(crate::scrub::plain("HTTP 502"));
        o.last_error_at = Some("2026-11-02T10:00:00Z".into());
        store.save_month(&o).unwrap();
        n.last_error = None;
        n.last_error_at = None;
        store.save_month(&n).unwrap();
        let r = build(&store, range, None).unwrap();
        let p = &r.platforms[0];
        assert!(p.last_error.is_none(), "{:?}", p.last_error);
        assert_eq!(p.status_month, None);
        assert_eq!(p.credits_remaining, Some(dec("9")));
        assert!(!to_text(&r).contains("last error"));

        // The newest attempt found a requirement: only that is reported, not an older error too.
        o.last_error = None;
        o.requirement = Some(crate::scrub::plain("an Admin key"));
        o.last_error_at = Some("2026-11-06T10:00:00Z".into());
        store.save_month(&o).unwrap();
        n.last_error = Some(crate::scrub::plain("HTTP 500"));
        n.last_error_at = Some("2026-11-05T12:00:00Z".into());
        store.save_month(&n).unwrap();
        let r = build(&store, range, None).unwrap();
        let p = &r.platforms[0];
        assert_eq!(
            p.requirement.as_ref().map(|e| e.as_str()),
            Some("an Admin key")
        );
        assert!(p.last_error.is_none(), "{:?}", p.last_error);
        assert_eq!(p.status_month, Some(oct));
        let text = to_text(&r);
        assert!(text.contains("needs (2026-10): an Admin key"), "{text}");
        assert!(!text.contains("last error"), "{text}");

        // An unreadable month is left out with a warning rather than failing the report.
        std::fs::write(t.path().join("cache/openai/2026-10.json"), "not json").unwrap();
        let r = build(&store, range, None).unwrap();
        let p = &r.platforms[0];
        assert_eq!(
            p.missing_months,
            vec![Month::parse("2026-09").unwrap(), oct]
        );
        assert_eq!(p.warnings.len(), 1);
        assert!(
            p.warnings[0].starts_with("2026-10: unreadable cache"),
            "{:?}",
            p.warnings
        );
        assert_eq!(p.total, dec("2"));
        assert!(to_text(&r).contains("warning: 2026-10: unreadable cache"));
        std::fs::write(t.path().join("cache/openai/2026-11.json"), "{").unwrap();
        let r = build(&store, Range::month(nov), None).unwrap();
        assert_eq!(r.platforms[0].warnings.len(), 1);
        assert!(to_text(&r).contains("a cached month could not be read"));
    }

    /// A month `fetch::run` cached as before history from `first`.
    fn placeholder(platform: &str, month: &str, first: &str, at: &str) -> MonthCache {
        let mut c = MonthCache::new(
            platform,
            Month::parse(month).unwrap(),
            FigureKind::Estimated,
            Breakdown::Key,
        );
        c.history_from = Some(d(first));
        c.last_fetched = Some(at.into());
        c
    }

    #[test]
    fn a_billed_year_with_before_history_months_stays_billed() {
        let t = TempDir::new("report-before-year");
        let store = Store::new(t.path());
        for m in 1..=10 {
            store
                .save_month(&placeholder(
                    "openai",
                    &format!("2025-{m:02}"),
                    "2025-11-05",
                    "2026-11-06T10:00:00Z",
                ))
                .unwrap();
        }
        let mut nov = MonthCache::new(
            "openai",
            Month::parse("2025-11").unwrap(),
            FigureKind::Billed,
            Breakdown::Key,
        );
        nov.history_from = Some(d("2025-11-05"));
        nov.add(d("2025-11-10"), "k1", dec("2"));
        nov.last_fetched = Some("2026-11-06T09:00:00Z".into());
        store.save_month(&nov).unwrap();
        let mut dec_ = MonthCache::new(
            "openai",
            Month::parse("2025-12").unwrap(),
            FigureKind::Billed,
            Breakdown::Key,
        );
        dec_.add(d("2025-12-10"), "k1", dec("3"));
        dec_.last_fetched = Some("2026-11-06T09:00:00Z".into());
        store.save_month(&dec_).unwrap();
        let r = build(
            &store,
            Range::new(d("2025-01-01"), d("2025-12-31")).unwrap(),
            None,
        )
        .unwrap();
        let p = &r.platforms[0];
        assert_eq!(p.kind, FigureKind::Billed);
        assert_eq!(p.total, dec("5"));
        assert_eq!(p.history_from, Some(d("2025-11-05")));
        let before: Vec<String> = p.before_history.iter().map(|m| m.to_string()).collect();
        assert_eq!(before.len(), 10);
        assert_eq!(before[0], "2025-01");
        assert_eq!(before[9], "2025-10");
        assert!(p.missing_months.is_empty(), "{:?}", p.missing_months);
        assert_eq!(p.last_fetched.as_deref(), Some("2026-11-06T09:00:00Z"));
        let json: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(json["platforms"][0]["history_from"], "2025-11-05");
        assert_eq!(json["platforms"][0]["before_history"][0], "2025-01");
        assert!(to_text(&r).contains("$5.00  billed"), "{}", to_text(&r));
    }

    #[test]
    fn a_before_history_placeholder_blanks_no_credits_and_hides_no_error() {
        let t = TempDir::new("report-before-credits");
        let store = Store::new(t.path());
        store
            .save_month(&placeholder(
                "xai",
                "2026-10",
                "2026-11-01",
                "2026-11-06T12:00:00Z",
            ))
            .unwrap();
        let mut n = MonthCache::new(
            "xai",
            Month::parse("2026-11").unwrap(),
            FigureKind::Estimated,
            Breakdown::Key,
        );
        n.add(d("2026-11-02"), "x1", dec("1"));
        n.credits_remaining = Some(dec("4"));
        n.data_as_of = Some("2026-11-05T08:00:00Z".into());
        n.last_fetched = Some("2026-11-05T09:00:00Z".into());
        n.last_error = Some(crate::scrub::plain("HTTP 500"));
        n.last_error_at = Some("2026-11-05T10:00:00Z".into());
        store.save_month(&n).unwrap();
        let r = build(
            &store,
            Range::new(d("2026-10-01"), d("2026-11-30")).unwrap(),
            None,
        )
        .unwrap();
        let p = &r.platforms[0];
        assert_eq!(p.credits_remaining, Some(dec("4")));
        assert_eq!(p.data_as_of.as_deref(), Some("2026-11-05T08:00:00Z"));
        assert_eq!(p.last_fetched.as_deref(), Some("2026-11-05T09:00:00Z"));
        assert_eq!(p.last_error.as_ref().map(|e| e.as_str()), Some("HTTP 500"));
        assert_eq!(p.status_month, Month::parse("2026-11"));
        assert_eq!(p.before_history, vec![Month::parse("2026-10").unwrap()]);
        assert!(p.missing_months.is_empty());
    }

    #[test]
    fn an_all_before_history_range_shows_no_history_never_zero() {
        let t = TempDir::new("report-before-all");
        let store = Store::new(t.path());
        for m in ["2025-11", "2025-12"] {
            store
                .save_month(&placeholder(
                    "anthropic",
                    m,
                    "2026-01-01",
                    "2026-11-06T10:00:00Z",
                ))
                .unwrap();
        }
        let range = Range::new(d("2025-11-01"), d("2026-01-31")).unwrap();
        let r = build(&store, range, None).unwrap();
        let p = &r.platforms[0];
        assert_eq!(p.last_fetched, None);
        assert_eq!(p.total, Decimal::ZERO);
        assert!(p.items.is_empty());
        assert_eq!(p.history_from, Some(d("2026-01-01")));
        assert_eq!(p.before_history.len(), 2);
        assert_eq!(p.missing_months, vec![Month::parse("2026-01").unwrap()]);
        let json: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert!(json["platforms"][0]["last_fetched"].is_null());
        // An unfetched month in range outranks the history note.
        let text = to_text(&r);
        let row = text.lines().find(|l| l.starts_with("Anthropic")).unwrap();
        assert!(row.contains("-  not fetched yet"), "{text}");
        let before = Range::new(d("2025-11-01"), d("2025-12-31")).unwrap();
        let text = to_text(&build(&store, before, None).unwrap());
        let row = text.lines().find(|l| l.starts_with("Anthropic")).unwrap();
        assert!(row.contains("-  no history before 2026-01-01"), "{text}");
        assert!(!row.contains('$'), "{text}");
        // An unreadable cache outranks the history note too.
        std::fs::write(t.path().join("cache/anthropic/2025-12.json"), "{").unwrap();
        let text = to_text(&build(&store, before, None).unwrap());
        let row = text.lines().find(|l| l.starts_with("Anthropic")).unwrap();
        assert!(
            row.contains("-  a cached month could not be read"),
            "{text}"
        );
    }

    #[test]
    fn a_figure_in_another_month_never_hides_a_partial_floor() {
        let t = TempDir::new("report-partial-floor");
        let store = Store::new(t.path());
        let mut oct = MonthCache::new(
            "openai",
            Month::parse("2025-10").unwrap(),
            FigureKind::Billed,
            Breakdown::Key,
        );
        oct.add(d("2025-10-12"), "k1", dec("1"));
        oct.last_fetched = Some("2026-10-01T09:00:00Z".into());
        store.save_month(&oct).unwrap();
        let mut nov = MonthCache::new(
            "openai",
            Month::parse("2025-11").unwrap(),
            FigureKind::Billed,
            Breakdown::Key,
        );
        nov.history_from = Some(d("2025-11-06"));
        nov.add(d("2025-11-10"), "k1", dec("2"));
        nov.last_fetched = Some("2026-11-06T09:00:00Z".into());
        store.save_month(&nov).unwrap();
        let range = Range::new(d("2025-10-01"), d("2025-11-30")).unwrap();
        let p = &build(&store, range, None).unwrap().platforms[0];
        assert_eq!(p.total, dec("3"));
        assert_eq!(
            p.history_from,
            Some(d("2025-11-06")),
            "November 1-5 are missing"
        );
        let text = to_text(&build(&store, range, None).unwrap());
        assert!(text.contains("  history from 2025-11-06"), "{text}");
    }

    #[test]
    fn a_placeholder_gap_ends_at_a_later_kept_month() {
        let t = TempDir::new("report-kept-before");
        let store = Store::new(t.path());
        store
            .save_month(&placeholder(
                "openai",
                "2025-09",
                "2025-11-06",
                "2026-11-06T09:00:00Z",
            ))
            .unwrap();
        for (month, day) in [("2025-10", "2025-10-12"), ("2025-11", "2025-11-03")] {
            let mut c = MonthCache::new(
                "openai",
                Month::parse(month).unwrap(),
                FigureKind::Billed,
                Breakdown::Key,
            );
            c.add(d(day), "k1", dec("1"));
            c.last_fetched = Some("2026-10-01T09:00:00Z".into());
            store.save_month(&c).unwrap();
        }
        let range = Range::new(d("2025-09-01"), d("2025-11-30")).unwrap();
        let p = &build(&store, range, None).unwrap().platforms[0];
        assert_eq!(p.total, dec("2"));
        assert_eq!(
            p.history_from,
            Some(d("2025-10-01")),
            "October holds figures"
        );
        // A successfully fetched October with no spend ends the gap just the same.
        let mut zero = MonthCache::new(
            "openai",
            Month::parse("2025-10").unwrap(),
            FigureKind::Billed,
            Breakdown::Key,
        );
        zero.last_fetched = Some("2026-10-01T09:00:00Z".into());
        store.save_month(&zero).unwrap();
        let p = &build(&store, range, None).unwrap().platforms[0];
        assert_eq!(p.total, dec("1"));
        assert_eq!(
            p.history_from,
            Some(d("2025-10-01")),
            "a $0.00 October is known"
        );
    }

    #[test]
    fn an_empty_partial_month_keeps_its_own_floor() {
        let t = TempDir::new("report-empty-partial");
        let store = Store::new(t.path());
        // October's placeholder was written later, when the floor had moved on to December.
        store
            .save_month(&placeholder(
                "openai",
                "2025-10",
                "2025-12-01",
                "2026-11-06T09:00:00Z",
            ))
            .unwrap();
        let mut nov = MonthCache::new(
            "openai",
            Month::parse("2025-11").unwrap(),
            FigureKind::Billed,
            Breakdown::Key,
        );
        nov.history_from = Some(d("2025-11-06"));
        nov.last_fetched = Some("2026-11-06T09:00:00Z".into());
        store.save_month(&nov).unwrap();
        let range = Range::new(d("2025-10-01"), d("2025-11-30")).unwrap();
        let p = &build(&store, range, None).unwrap().platforms[0];
        assert_eq!(p.before_history, vec![Month::parse("2025-10").unwrap()]);
        assert_eq!(
            p.history_from,
            Some(d("2025-11-06")),
            "November's figures start at its floor, not its first day"
        );
    }

    #[test]
    fn json_and_text_shapes() {
        let t = TempDir::new("report-json");
        let store = Store::new(t.path());
        seed(&store);
        let r = build(&store, Range::month(Month::parse("2026-11").unwrap()), None).unwrap();
        let json: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(json["total"], "13.455");
        assert_eq!(json["from"], "2026-11-01");
        assert_eq!(json["platforms"][0]["kind"], "billed");
        let text = to_text(&r);
        assert!(text.contains("$10.16"), "{text}");
        assert!(text.contains("$13.46"), "{text}");
    }

    #[test]
    fn terminal_text_escapes_ansi_sequences_and_json_keeps_them() {
        let t = TempDir::new("report-ansi");
        let store = Store::new(t.path());
        let nov = Month::parse("2026-11").unwrap();
        let mut m = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        m.add(d("2026-11-01"), "k1", dec("1"));
        m.names.insert("k1".into(), "Work\u{1b}[2J".into());
        m.last_fetched = Some("2026-11-01T10:00:00Z".into());
        m.last_error = Some(crate::scrub::plain("HTTP 500 \u{1b}]0;title\u{7}\u{9b}31m"));
        m.last_error_at = Some("2026-11-02T10:00:00Z".into());
        store.save_month(&m).unwrap();
        let r = build(&store, Range::month(nov), None).unwrap();
        let text = to_text(&r);
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        assert!(text.contains("Work\\u{1b}[2J"), "{text}");
        assert!(
            text.contains("HTTP 500 \\u{1b}]0;title\\u{7}\\u{9b}31m"),
            "{text}"
        );
        let json: serde_json::Value = serde_json::from_str(&to_json(&r)).unwrap();
        assert_eq!(json["platforms"][0]["items"][0]["name"], "Work\u{1b}[2J");
        assert_eq!(
            json["platforms"][0]["last_error"],
            "HTTP 500 \u{1b}]0;title\u{7}\u{9b}31m"
        );
    }

    #[test]
    fn terminal_text_keeps_a_multi_line_error_on_separate_lines() {
        let t = TempDir::new("report-lines");
        let store = Store::new(t.path());
        let nov = Month::parse("2026-11").unwrap();
        let mut m = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        m.add(d("2026-11-01"), "k1", dec("1"));
        m.last_fetched = Some("2026-11-01T10:00:00Z".into());
        m.last_error = Some(crate::scrub::plain(
            "HTTP 500: {\n  \"error\": \"x\u{1b}[2J\"\n}",
        ));
        m.last_error_at = Some("2026-11-02T10:00:00Z".into());
        store.save_month(&m).unwrap();
        let r = build(&store, Range::month(nov), None).unwrap();
        let text = to_text(&r);
        assert!(text.contains("HTTP 500: {\n"), "{text}");
        assert!(text.contains("\"error\": \"x\\u{1b}[2J\""), "{text}");
        assert!(!text.contains("\\n"), "{text}");
    }
}
