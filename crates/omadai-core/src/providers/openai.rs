//! OpenAI: billed dollars per API key from the organization costs endpoint, read with an Admin key.

use super::{
    Dec, UNATTRIBUTED, bearer, before_history, call_json, check_outcome, enc, fetch_each,
    next_cursor, paged, unix, usd, window,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Date, Month};
use crate::http::{Request, Transport};
use crate::provider::{Cause, CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use serde::Deserialize;
use std::collections::BTreeMap;

const BASE: &str = "https://api.openai.com/v1/organization";
/// The costs endpoint caps a page at 31 daily buckets.
const LIMIT: u32 = 31;
const LOOKBACK_CODE: &str = "reporting_lookback_exceeded";
const EARLIEST_PROSE: &str = "Earliest supported start date: ";

pub struct OpenAi {
    pub today: fn() -> Date,
}

impl Default for OpenAi {
    fn default() -> Self {
        OpenAi {
            today: Date::today_utc,
        }
    }
}

#[derive(Deserialize)]
struct CostsPage {
    #[serde(default)]
    data: Vec<Bucket>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_page: Option<String>,
}

#[derive(Deserialize)]
struct Bucket {
    start_time: i64,
    /// Absent on a day with no spend.
    #[serde(default)]
    results: Vec<CostResult>,
}

#[derive(Deserialize)]
struct CostResult {
    #[serde(default)]
    api_key_id: Option<String>,
    #[serde(default)]
    amount: Option<Amount>,
    #[serde(default)]
    organization_id: Option<String>,
}

#[derive(Deserialize)]
struct Amount {
    #[serde(default)]
    value: Option<Dec>,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Deserialize)]
struct List {
    #[serde(default)]
    data: Vec<Listed>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct Listed {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

fn get(url: String, secret: &Secret) -> Request {
    Request::get(url).secret_header("Authorization", bearer(secret))
}

fn costs(
    http: &dyn Transport,
    secret: &Secret,
    start: Date,
    end: Date,
    limit: u32,
    mut each: impl FnMut(Bucket) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    paged(
        |cursor| {
            let mut url = format!(
                "{BASE}/costs?start_time={}&end_time={}&group_by=api_key_id&limit={limit}",
                unix(start),
                unix(end)
            );
            if let Some(c) = cursor {
                url.push_str(&format!("&page={}", enc(c)));
            }
            call_json::<CostsPage>(http, &get(url, secret), secret)
                .map_err(|e| e.into_provider(secret))
        },
        |p| next_cursor(p.has_more, p.next_page.as_ref()),
        |p| p.data.into_iter().try_for_each(&mut each),
    )
}

/// Every page of an `after`-cursor listing.
fn list(http: &dyn Transport, secret: &Secret, base: &str) -> Result<Vec<Listed>, ProviderError> {
    let mut out = Vec::new();
    paged(
        |cursor| {
            let mut url = format!(
                "{base}{}limit=100",
                if base.contains('?') { "&" } else { "?" }
            );
            if let Some(c) = cursor {
                url.push_str(&format!("&after={}", enc(c)));
            }
            call_json::<List>(http, &get(url, secret), secret).map_err(|e| e.into_provider(secret))
        },
        |p| next_cursor(p.has_more, p.last_id.as_ref()),
        |p| {
            out.extend(p.data);
            Ok(())
        },
    )?;
    Ok(out)
}

/// Names for admin keys and every project's keys, archived projects included.
fn key_names(http: &dyn Transport, secret: &Secret) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    // Each listing is best effort, so one refused listing never discards the names already found.
    let mut keep = |url: String| {
        for k in list(http, secret, &url).unwrap_or_default() {
            if let Some(n) = k.name.filter(|n| !n.is_empty()) {
                names.insert(k.id, n);
            }
        }
    };
    keep(format!("{BASE}/admin_api_keys"));
    // Archived projects are left out: OpenAI refuses to list their keys (400 project_archived).
    for project in list(http, secret, &format!("{BASE}/projects")).unwrap_or_default() {
        keep(format!("{BASE}/projects/{}/api_keys", enc(&project.id)));
    }
    names
}

/// The first day a lookback refusal names, or `fallback`; the only vendor prose omadai parses.
fn lookback_start(e: &ProviderError, fallback: Date) -> Option<Date> {
    let text = e.message.as_str();
    if e.cause != Cause::Status(400) || !text.contains(LOOKBACK_CODE) {
        return None;
    }
    Some(
        text.split_once(EARLIEST_PROSE)
            .and_then(|(_, rest)| rest.get(..10))
            .and_then(Date::parse)
            .unwrap_or(fallback),
    )
}

/// The same date a year earlier (29 February becomes the 28th): the costs endpoint keeps a rolling
/// year, and a floor never later than the vendor's loses no day.
fn year_before(today: Date) -> Date {
    let (year, month) = (today.year - 1, today.month);
    Date::new(
        year,
        month,
        today.day.min(crate::date::days_in_month(year, month)),
    )
    .expect("a day clamped to its month")
}

impl OpenAi {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
    ) -> Result<MonthCache, ProviderError> {
        let secret = &credential.secret;
        let today = (self.today)();
        let earliest = year_before(today);
        if month.last_day() < earliest {
            return Err(before_history(earliest));
        }
        let (first, end) = window(month, today)?;
        let start = first.max(earliest);
        let from = (start > first).then_some(start);
        match Self::read(http, secret, month, start, end, from) {
            Err(e) => match lookback_start(&e, earliest) {
                Some(day) if day > month.last_day() => Err(before_history(day)),
                // Only a later start can succeed, so a refusal naming this start or an earlier one stands.
                Some(day) if day > start => Self::read(http, secret, month, day, end, Some(day)),
                _ => Err(e),
            },
            ok => ok,
        }
    }

    fn read(
        http: &dyn Transport,
        secret: &Secret,
        month: Month,
        start: Date,
        end: Date,
        history_from: Option<Date>,
    ) -> Result<MonthCache, ProviderError> {
        let mut cache = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
        cache.history_from = history_from;
        costs(http, secret, start, end, LIMIT, |bucket| {
            let day = Date::from_days(bucket.start_time.div_euclid(86_400));
            for r in bucket.results {
                let Some(Amount {
                    value: Some(Dec(value)),
                    currency,
                }) = r.amount
                else {
                    continue;
                };
                if let Some(c) = &currency {
                    usd(c)?;
                }
                // Keyed by organization, so two credentials for one organization merge as one row.
                let id = match (r.api_key_id, r.organization_id) {
                    (Some(key), _) => key,
                    (None, Some(org)) => format!("{UNATTRIBUTED} ({org})"),
                    (None, None) => UNATTRIBUTED.into(),
                };
                cache.add(day, &id, value);
            }
            Ok(())
        })?;
        let ids: Vec<&String> = cache.daily.values().flat_map(|m| m.keys()).collect();
        if ids.iter().any(|id| !id.starts_with(UNATTRIBUTED)) {
            // Names are a convenience: a failed listing leaves IDs unnamed rather than losing figures.
            let wanted: Vec<String> = ids.into_iter().cloned().collect();
            cache.names = key_names(http, secret)
                .into_iter()
                .filter(|(id, _)| wanted.contains(id))
                .collect();
        }
        Ok(cache)
    }
}

impl Provider for OpenAi {
    fn platform(&self) -> &'static str {
        "openai"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        let today = (self.today)();
        let yesterday = Date::from_days(today.to_days() - 1);
        let mut days = 0;
        check_outcome(
            costs(http, credential, yesterday, today, 1, |_| {
                days += 1;
                Ok(())
            })
            .map(|()| format!("read {days} day of organization costs")),
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
    use crate::http::Method;
    use crate::http::mock::MockSeq;

    const KEY: &str = "fk-openai-admin-TESTKEY24680";
    const PAGE1: &str = include_str!("../../fixtures/openai/costs_page1.json");
    const PAGE2: &str = include_str!("../../fixtures/openai/costs_page2.json");
    const ADMIN_KEYS: &str = include_str!("../../fixtures/openai/admin_api_keys.json");
    const PROJECTS: &str = include_str!("../../fixtures/openai/projects.json");
    const PROJECT_KEYS: &str = include_str!("../../fixtures/openai/project_api_keys.json");
    const ERR_401: &str = include_str!("../../fixtures/openai/error_401.json");

    #[test]
    fn the_floor_is_the_same_date_a_year_earlier() {
        let d = |s| Date::parse(s).unwrap();
        assert_eq!(year_before(d("2026-11-06")), d("2025-11-06"));
        assert_eq!(
            year_before(d("2028-03-01")),
            d("2027-03-01"),
            "not 365 days"
        );
        assert_eq!(year_before(d("2028-02-29")), d("2027-02-28"));
    }

    fn provider() -> OpenAi {
        OpenAi {
            today: || Date::parse("2026-11-05").unwrap(),
        }
    }

    fn nov() -> Month {
        Month::parse("2026-11").unwrap()
    }

    fn happy() -> MockSeq {
        MockSeq::new(vec![
            ("/costs?", 200, PAGE1.into()),
            ("/costs?", 200, PAGE2.into()),
            ("/admin_api_keys?", 200, ADMIN_KEYS.into()),
            ("/projects?", 200, PROJECTS.into()),
            ("/projects/proj_fk01/api_keys?", 200, PROJECT_KEYS.into()),
        ])
    }

    #[test]
    fn pages_buckets_and_names() {
        let mock = happy();
        let c = provider()
            .fetch_month(
                &guarded(&mock),
                &[cred("work", KEY)],
                nov(),
                &Config::default(),
            )
            .unwrap();
        assert_eq!(c.kind, FigureKind::Billed);
        assert_eq!(c.daily.len(), 3, "the day without results adds nothing");
        assert_eq!(c.daily[&d("2026-11-01")]["key_fk_alpha"], dec("1.2345"));
        assert_eq!(c.daily[&d("2026-11-04")]["key_fk_beta"], dec("0.1"));
        assert_eq!(
            c.daily[&d("2026-11-04")]["unattributed (org-fk-example)"],
            dec("0.02")
        );
        assert_eq!(total(&c), dec("4.3545"));
        assert_eq!(c.names["key_fk_alpha"], "Admin reporting");
        assert_eq!(c.names["key_fk_beta"], "Render bot");
        assert!(
            !c.names.contains_key("key_fk_unused"),
            "only keys with spend are named"
        );
        let sent = mock.sent();
        assert_eq!(sent.len(), 5);
        assert_eq!(
            sent[0].url,
            "https://api.openai.com/v1/organization/costs?start_time=1793491200&end_time=1793923200&group_by=api_key_id&limit=31"
        );
        assert!(
            sent[1].url.ends_with("&page=fk_page_2%3D%3D"),
            "{}",
            sent[1].url
        );
        assert!(
            !sent[3].url.contains("include_archived"),
            "archived projects cannot list keys"
        );
        for s in &sent {
            assert_eq!(s.method, Method::Get);
            assert_eq!(
                s.header("Authorization"),
                Some(format!("Bearer {KEY}").as_str())
            );
        }
    }

    #[test]
    fn names_failure_keeps_figures() {
        let mock = MockSeq::new(vec![
            ("/costs?", 200, PAGE1.into()),
            ("/costs?", 200, PAGE2.into()),
            ("/admin_api_keys?", 500, "{}".into()),
        ]);
        let c = provider()
            .fetch_month(
                &guarded(&mock),
                &[cred("work", KEY)],
                nov(),
                &Config::default(),
            )
            .unwrap();
        assert_eq!(total(&c), dec("4.3545"));
        assert!(c.names.is_empty());
    }

    #[test]
    fn a_refused_project_listing_keeps_the_other_names() {
        let two = PROJECTS.replace(
            r#"{ "object": "organization.project", "id": "proj_fk01""#,
            r#"{ "object": "organization.project", "id": "proj_fk00", "name": "Gone", "status": "active" },
    { "object": "organization.project", "id": "proj_fk01""#,
        );
        let mock = MockSeq::new(vec![
            ("/costs?", 200, PAGE1.into()),
            ("/costs?", 200, PAGE2.into()),
            ("/admin_api_keys?", 200, ADMIN_KEYS.into()),
            ("/projects?", 200, two),
            (
                "/projects/proj_fk00/api_keys?",
                400,
                r#"{"error":{"code":"project_archived"}}"#.into(),
            ),
            ("/projects/proj_fk01/api_keys?", 200, PROJECT_KEYS.into()),
        ]);
        let c = provider()
            .fetch_month(
                &guarded(&mock),
                &[cred("work", KEY)],
                nov(),
                &Config::default(),
            )
            .unwrap();
        assert_eq!(c.names["key_fk_alpha"], "Admin reporting");
        assert_eq!(c.names["key_fk_beta"], "Render bot");
    }

    #[test]
    fn unauthorized_body_with_key_is_scrubbed() {
        let mock = MockSeq::new(vec![("/costs?", 401, ERR_401.replace("{KEY}", KEY))]);
        let e = provider()
            .fetch_month(
                &guarded(&mock),
                &[cred("work", KEY)],
                nov(),
                &Config::default(),
            )
            .unwrap_err();
        assert_no_key(&e, KEY);
        assert!(e.message.as_str().contains("HTTP 401"), "{}", e.message);
        assert!(e.message.as_str().contains("[redacted]"), "{}", e.message);
        assert!(
            e.message.as_str().starts_with("key \"work\": "),
            "{}",
            e.message
        );
        assert_eq!(e.requirement, None);
        match provider().check(
            &guarded(&MockSeq::new(vec![(
                "/costs?",
                401,
                ERR_401.replace("{KEY}", KEY),
            )])),
            &Secret::new(KEY.into()),
            &Config::default(),
        ) {
            CheckOutcome::Failed(m) => assert!(!m.as_str().contains(KEY), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn redirect_is_refused() {
        let mock = MockSeq::new(vec![("/costs?", 302, String::new())]);
        let e = provider()
            .fetch_month(
                &guarded(&mock),
                &[cred("work", KEY)],
                nov(),
                &Config::default(),
            )
            .unwrap_err();
        assert!(
            e.message.as_str().contains("redirect refused"),
            "{}",
            e.message
        );
        assert_eq!(mock.sent().len(), 1);
    }

    const LOOKBACK: &str = include_str!("../../fixtures/openai/error_400_lookback.json");
    const EMPTY: &str = r#"{"object":"page","has_more":false,"data":[]}"#;

    fn fetch(mock: &MockSeq, month: &str) -> Result<MonthCache, ProviderError> {
        provider().fetch_month(
            &guarded(mock),
            &[cred("work", KEY)],
            Month::parse(month).unwrap(),
            &Config::default(),
        )
    }

    fn start_of(url: &str) -> Date {
        let secs: i64 = url
            .split("start_time=")
            .nth(1)
            .and_then(|r| r.split('&').next())
            .and_then(|n| n.parse().ok())
            .unwrap();
        Date::from_days(secs / 86_400)
    }

    #[test]
    fn a_month_before_the_year_makes_no_request() {
        let mock = MockSeq::new(vec![]);
        let e = fetch(&mock, "2025-10").unwrap_err();
        assert_eq!(e.before_history, Some(d("2025-11-05")), "{e:?}");
        assert_eq!(e.requirement, None);
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn the_boundary_month_requests_from_the_earliest_day() {
        let mock = MockSeq::new(vec![("/costs?", 200, EMPTY.into())]);
        let c = fetch(&mock, "2025-11").unwrap();
        assert_eq!(c.history_from, Some(d("2025-11-05")));
        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(start_of(&sent[0].url), d("2025-11-05"));
        assert!(
            sent[0]
                .url
                .contains(&format!("end_time={}", unix(d("2025-12-01"))))
        );
        // A month wholly inside the year starts on its first day and has no history mark.
        let mock = MockSeq::new(vec![("/costs?", 200, EMPTY.into())]);
        let c = fetch(&mock, "2025-12").unwrap();
        assert_eq!(c.history_from, None);
        assert_eq!(start_of(&mock.sent()[0].url), d("2025-12-01"));
    }

    #[test]
    fn a_lookback_error_inside_the_month_retries_once_from_its_date() {
        let mock = MockSeq::new(vec![
            ("/costs?", 400, LOOKBACK.into()),
            ("/costs?", 200, EMPTY.into()),
        ]);
        let c = fetch(&mock, "2025-11").unwrap();
        assert_eq!(c.history_from, Some(d("2025-11-06")));
        let sent = mock.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(start_of(&sent[0].url), d("2025-11-05"));
        assert_eq!(start_of(&sent[1].url), d("2025-11-06"));

        // The retry is the only one: a second refusal is the month's error.
        let mock = MockSeq::new(vec![
            ("/costs?", 400, LOOKBACK.into()),
            ("/costs?", 400, LOOKBACK.into()),
        ]);
        let e = fetch(&mock, "2025-11").unwrap_err();
        assert_eq!(e.before_history, None);
        assert_eq!(e.cause, crate::provider::Cause::Status(400));
        assert_eq!(mock.sent().len(), 2);
    }

    #[test]
    fn a_lookback_error_after_the_month_is_before_history() {
        let later = LOOKBACK.replace("2025-11-06", "2025-12-01");
        let mock = MockSeq::new(vec![("/costs?", 400, later)]);
        let e = fetch(&mock, "2025-11").unwrap_err();
        assert_eq!(e.before_history, Some(d("2025-12-01")), "{e:?}");
        assert_eq!(mock.sent().len(), 1);
        // With no date in the body the local earliest day stands, which this request already used.
        let undated = LOOKBACK.replace("Earliest supported start date: 2025-11-06 (UTC).", "");
        let mock = MockSeq::new(vec![("/costs?", 400, undated)]);
        let e = fetch(&mock, "2025-11").unwrap_err();
        assert_eq!(e.before_history, None, "{e:?}");
        assert_eq!(mock.sent().len(), 1);
    }

    #[test]
    fn check_reads_one_day() {
        let mock = MockSeq::new(vec![("/costs?", 200, PAGE2.into())]);
        let out = provider().check(
            &guarded(&mock),
            &Secret::new(KEY.into()),
            &Config::default(),
        );
        assert!(matches!(out, CheckOutcome::Ok(_)), "{out:?}");
        assert!(
            mock.sent()[0]
                .url
                .contains("start_time=1793750400&end_time=1793836800")
        );
        assert!(mock.sent()[0].url.contains("limit=1"));
    }
}
