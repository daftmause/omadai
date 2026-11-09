//! Anthropic: billed dollars per API key from the analytics cost endpoint, read with an Admin key.

use super::{
    CallError, Dec, UNATTRIBUTED, call_json, cents_to_dollars, check_outcome, day_of, enc,
    fetch_each, next_cursor, paged, requirement, usd, window,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Date, Month};
use crate::http::{HttpError, Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use serde::Deserialize;

const BASE: &str = "https://api.anthropic.com/v1/organizations";
const VERSION: &str = "2023-06-01";
const BETA: &str = "analytics-usage-cost-2026-08-24";
/// The longest range one cost request may span.
const MAX_DAYS: i64 = 31;
/// The cost endpoint holds nothing before this month.
const FIRST_MONTH: Month = Month {
    year: 2026,
    month: 1,
};

pub struct Anthropic {
    pub today: fn() -> Date,
}

impl Default for Anthropic {
    fn default() -> Self {
        Anthropic {
            today: Date::today_utc,
        }
    }
}

#[derive(Deserialize)]
struct CostPage {
    #[serde(default)]
    data: Vec<Row>,
    #[serde(default)]
    data_refreshed_at: Option<String>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_page: Option<String>,
}

#[derive(Deserialize)]
struct Row {
    starting_at: String,
    #[serde(default)]
    group: Group,
    cost: Cost,
}

#[derive(Deserialize, Default)]
struct Group {
    #[serde(default)]
    api_key_id: Option<String>,
}

#[derive(Deserialize)]
struct Cost {
    /// Cents, as a decimal string.
    amount: Dec,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Deserialize)]
struct KeyList {
    #[serde(default)]
    data: Vec<Key>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct Key {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

/// Splits `[from, to)` into consecutive ranges of at most `max_days` days.
pub fn chunks(from: Date, to: Date, max_days: i64) -> Vec<(Date, Date)> {
    let mut out = Vec::new();
    let mut start = from.to_days();
    let end = to.to_days();
    while start < end {
        let stop = (start + max_days).min(end);
        out.push((Date::from_days(start), Date::from_days(stop)));
        start = stop;
    }
    out
}

fn request(url: String, secret: &Secret, beta: bool) -> Request {
    let r = Request::get(url)
        .secret_header("x-api-key", Secret::with_prefix("", secret))
        .header("anthropic-version", VERSION);
    if beta {
        r.header("anthropic-beta", BETA)
    } else {
        r
    }
}

/// A 403 from the Admin API: what an individual account or a non-admin key gets.
fn map_error(e: CallError, secret: &Secret) -> ProviderError {
    match e {
        CallError::Http(HttpError::Status { status, body }) if status == 403 => {
            requirement(format!(
                "Anthropic spend needs an Admin key on a team (organization) Console account; \
                 individual accounts have no Admin API. Set up an organization at \
                 https://platform.claude.com/settings/organization, then create a key under \
                 Settings > Admin keys. Anthropic replied HTTP {status}: {body}"
            ))
        }
        other => other.into_provider(secret),
    }
}

fn cost_range(
    http: &dyn Transport,
    secret: &Secret,
    from: Date,
    to: Date,
    mut each: impl FnMut(CostPage) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    for (start, end) in chunks(from, to, MAX_DAYS) {
        paged(
            |cursor| {
                let mut url = format!(
                    "{BASE}/analytics/cost?starting_at={start}T00:00:00Z&ending_at={end}T00:00:00Z\
                     &group_by[]=api_key_id&time_bucket_width=1d&limit=1000"
                );
                if let Some(c) = cursor {
                    url.push_str(&format!("&page={}", enc(c)));
                }
                call_json::<CostPage>(http, &request(url, secret, true), secret)
                    .map_err(|e| map_error(e, secret))
            },
            |p| next_cursor(p.has_more, p.next_page.as_ref()),
            &mut each,
        )?;
    }
    Ok(())
}

fn key_names(
    http: &dyn Transport,
    secret: &Secret,
    cache: &mut MonthCache,
) -> Result<(), ProviderError> {
    let wanted: Vec<String> = cache
        .daily
        .values()
        .flat_map(|m| m.keys().cloned())
        .collect();
    let names = &mut cache.names;
    paged(
        |cursor| {
            let mut url = format!("{BASE}/api_keys?limit=1000");
            if let Some(c) = cursor {
                url.push_str(&format!("&after_id={}", enc(c)));
            }
            call_json::<KeyList>(http, &request(url, secret, false), secret)
                .map_err(|e| e.into_provider(secret))
        },
        |p| next_cursor(p.has_more, p.last_id.as_ref()),
        |p| {
            for k in p.data {
                if let Some(n) = k.name.filter(|n| !n.is_empty())
                    && wanted.contains(&k.id)
                {
                    names.insert(k.id, n);
                }
            }
            Ok(())
        },
    )
}

impl Anthropic {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
    ) -> Result<MonthCache, ProviderError> {
        if month < FIRST_MONTH {
            return Err(super::before_history(FIRST_MONTH.first_day()));
        }
        let secret = &credential.secret;
        let (start, end) = window(month, (self.today)())?;
        let mut cache = MonthCache::new("anthropic", month, FigureKind::Billed, Breakdown::Key);
        cost_range(http, secret, start, end, |page| {
            // Across chunks and pages the earliest refresh bounds how current the month is.
            if let Some(at) = page.data_refreshed_at {
                cache.data_as_of = Some(match cache.data_as_of.take() {
                    Some(old) => old.min(at),
                    None => at,
                });
            }
            for row in page.data {
                if let Some(c) = &row.cost.currency {
                    usd(c)?;
                }
                let id = row.group.api_key_id.unwrap_or_else(|| UNATTRIBUTED.into());
                cache.add(
                    day_of(&row.starting_at)?,
                    &id,
                    cents_to_dollars(row.cost.amount.0),
                );
            }
            Ok(())
        })?;
        if cache
            .daily
            .values()
            .flat_map(|m| m.keys())
            .any(|id| id != UNATTRIBUTED)
        {
            // Names are a convenience: a failed listing leaves IDs unnamed rather than losing figures.
            let mut named = cache.clone();
            if key_names(http, secret, &mut named).is_ok() {
                cache.names = named.names;
            }
        }
        Ok(cache)
    }
}

impl Provider for Anthropic {
    fn platform(&self) -> &'static str {
        "anthropic"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        let today = (self.today)();
        let yesterday = Date::from_days(today.to_days() - 1);
        let mut rows = 0;
        check_outcome(
            cost_range(http, credential, yesterday, today, |p| {
                rows += p.data.len();
                Ok(())
            })
            .map(|()| format!("read yesterday's cost report ({rows} rows)")),
        )
    }

    fn fetch_month(
        &self,
        http: &dyn Transport,
        credentials: &[Credential],
        month: Month,
        _config: &Config,
    ) -> Result<MonthCache, ProviderError> {
        fetch_each(credentials, |c| self.one(http, c, month))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::http::mock::MockSeq;

    const KEY: &str = "fk-anthropic-admin-TESTKEY13579";
    const PAGE1: &str = include_str!("../../fixtures/anthropic/cost_page1.json");
    const PAGE2: &str = include_str!("../../fixtures/anthropic/cost_page2.json");
    const KEYS: &str = include_str!("../../fixtures/anthropic/api_keys.json");
    const ERR_403: &str = include_str!("../../fixtures/anthropic/error_403.json");
    const ERR_401: &str = include_str!("../../fixtures/anthropic/error_401.json");

    fn provider(today: &'static str) -> Anthropic {
        let today: fn() -> Date = match today {
            "2026-11-05" => || Date::parse("2026-11-05").unwrap(),
            _ => || Date::parse("2026-12-03").unwrap(),
        };
        Anthropic { today }
    }

    fn fetch(
        mock: &MockSeq,
        month: &str,
        today: &'static str,
    ) -> Result<MonthCache, ProviderError> {
        provider(today).fetch_month(
            &guarded(mock),
            &[cred("org", KEY)],
            Month::parse(month).unwrap(),
            &Config::default(),
        )
    }

    #[test]
    fn chunks_split_at_the_limit() {
        assert_eq!(
            chunks(d("2026-10-01"), d("2026-11-01"), MAX_DAYS),
            vec![(d("2026-10-01"), d("2026-11-01"))],
            "a 31-day month is one request"
        );
        assert_eq!(
            chunks(d("2026-10-01"), d("2026-11-01"), 10),
            vec![
                (d("2026-10-01"), d("2026-10-11")),
                (d("2026-10-11"), d("2026-10-21")),
                (d("2026-10-21"), d("2026-10-31")),
                (d("2026-10-31"), d("2026-11-01")),
            ]
        );
        assert!(chunks(d("2026-10-01"), d("2026-10-01"), 10).is_empty());
        for (a, b) in chunks(d("2026-01-01"), d("2026-12-31"), MAX_DAYS) {
            assert!(b.to_days() - a.to_days() <= MAX_DAYS);
        }
    }

    #[test]
    fn closed_month_pages_and_converts_cents_exactly() {
        let mock = MockSeq::new(vec![
            ("/analytics/cost?", 200, PAGE1.into()),
            ("/analytics/cost?", 200, PAGE2.into()),
            ("/api_keys?", 200, KEYS.into()),
        ]);
        let c = fetch(&mock, "2026-10", "2026-12-03").unwrap();
        let sent = mock.sent();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[0].url,
            "https://api.anthropic.com/v1/organizations/analytics/cost?starting_at=2026-10-01T00:00:00Z&ending_at=2026-11-01T00:00:00Z&group_by[]=api_key_id&time_bucket_width=1d&limit=1000"
        );
        assert!(
            sent[1].url.ends_with("&page=fk_cursor_2"),
            "{}",
            sent[1].url
        );
        assert_eq!(sent[0].header("x-api-key"), Some(KEY));
        assert_eq!(sent[0].header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(sent[0].header("anthropic-beta"), Some(BETA));
        assert_eq!(sent[2].header("anthropic-beta"), None);
        assert_eq!(c.daily[&d("2026-10-02")]["apikey_fk_one"], dec("412.8"));
        assert_eq!(c.daily[&d("2026-10-02")]["apikey_fk_two"], dec("0.000001"));
        assert_eq!(c.daily[&d("2026-10-31")]["apikey_fk_one"], dec("0.1"));
        assert_eq!(
            c.daily[&d("2026-10-31")]["unattributed (org)"],
            dec("12.345678")
        );
        assert_eq!(total(&c), dec("425.245679"));
        assert_eq!(c.data_as_of.as_deref(), Some("2026-12-02T20:00:00Z"));
        assert_eq!(c.names["apikey_fk_one"], "Pipeline");
        assert_eq!(c.names.len(), 2);
    }

    #[test]
    fn running_month_ends_after_today() {
        let mock = MockSeq::new(vec![("/analytics/cost?", 200, PAGE2.into())]);
        fetch(&mock, "2026-11", "2026-11-05").unwrap();
        assert!(
            mock.sent()[0]
                .url
                .contains("starting_at=2026-11-01T00:00:00Z&ending_at=2026-11-06T00:00:00Z")
        );
    }

    #[test]
    fn not_found_is_an_error_not_a_requirement() {
        let mock = MockSeq::new(vec![(
            "/analytics/cost?",
            404,
            ERR_403.replace("{KEY}", KEY),
        )]);
        let e = fetch(&mock, "2026-10", "2026-12-03").unwrap_err();
        assert_no_key(&e, KEY);
        assert_eq!(e.requirement, None);
        assert!(e.message.as_str().contains("HTTP 404"), "{}", e.message);
    }

    #[test]
    fn individual_account_is_a_requirement_row() {
        let status = 403;
        let mock = MockSeq::new(vec![(
            "/analytics/cost?",
            status,
            ERR_403.replace("{KEY}", KEY),
        )]);
        let e = fetch(&mock, "2026-10", "2026-12-03").unwrap_err();
        assert_no_key(&e, KEY);
        let r = e.requirement.clone().expect("requirement row");
        assert!(r.contains("team (organization) Console account"), "{r}");
        assert!(r.contains(&format!("HTTP {status}")), "{r}");
        assert!(r.contains("[redacted]"), "{r}");
        let check = provider("2026-11-05").check(
            &guarded(&MockSeq::new(vec![(
                "/analytics/cost?",
                status,
                ERR_403.into(),
            )])),
            &Secret::new(KEY.into()),
            &Config::default(),
        );
        assert!(matches!(check, CheckOutcome::Requirement(_)), "{check:?}");
    }

    #[test]
    fn unauthorized_is_an_error_without_the_key() {
        let mock = MockSeq::new(vec![(
            "/analytics/cost?",
            401,
            ERR_401.replace("{KEY}", KEY),
        )]);
        let e = fetch(&mock, "2026-10", "2026-12-03").unwrap_err();
        assert_no_key(&e, KEY);
        assert_eq!(e.requirement, None);
        assert!(e.message.as_str().contains("HTTP 401"), "{}", e.message);
    }

    #[test]
    fn redirect_and_early_month_are_refused() {
        let mock = MockSeq::new(vec![("/analytics/cost?", 307, String::new())]);
        let e = fetch(&mock, "2026-10", "2026-12-03").unwrap_err();
        assert!(
            e.message.as_str().contains("redirect refused"),
            "{}",
            e.message
        );
        assert_eq!(mock.sent().len(), 1);
    }

    #[test]
    fn a_month_before_2026_is_before_history_without_a_request() {
        let none = MockSeq::new(vec![]);
        let e = fetch(&none, "2025-12", "2026-12-03").unwrap_err();
        assert_eq!(e.before_history, Some(d("2026-01-01")), "{e:?}");
        assert_eq!(e.requirement, None);
        assert!(none.sent().is_empty());
    }
}
