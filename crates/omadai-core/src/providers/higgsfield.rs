//! Higgsfield: per-key dollars from the models usage endpoint, which the platform labels an estimate.

use super::{
    Dec, MAX_PAGES, UNATTRIBUTED, call_json, check_outcome, day_of, fail, fetch_each, fingerprint,
    usd, window,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Date, Month};
use crate::http::{Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use serde::Deserialize;

const URL: &str = "https://api.higgsfield.ai/models/usage";
/// The largest page the docs allow; a 31-day window of day buckets fits in one.
const LIMIT: u32 = 100;

pub struct Higgsfield {
    pub today: fn() -> Date,
}

impl Default for Higgsfield {
    fn default() -> Self {
        Higgsfield {
            today: Date::today_utc,
        }
    }
}

#[derive(Deserialize)]
struct UsagePage {
    #[serde(default)]
    time_series: Vec<Bucket>,
}

#[derive(Hash, Deserialize)]
struct Bucket {
    bucket: String,
    #[serde(default)]
    results: Vec<Row>,
}

// Row fields follow the vendor's documented example; no live row has been seen yet.
#[derive(Hash, Deserialize)]
struct Row {
    cost: Dec,
    #[serde(default)]
    currency: Option<String>,
    /// The key ID with `expand=auth_method`; absent or null where no key is attributed.
    #[serde(default)]
    auth_method: Option<serde_json::Value>,
}

fn key_id(auth: Option<&serde_json::Value>) -> String {
    let id = match auth {
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        Some(serde_json::Value::Object(o)) => o
            .get("api_key_id")
            .or_else(|| o.get("id"))
            .and_then(|v| v.as_str()),
        _ => None,
    };
    id.filter(|s| !s.is_empty())
        .map_or_else(|| UNATTRIBUTED.to_string(), str::to_string)
}

/// The dashboard issues one `id:secret` string; either half alone is refused before any request.
fn well_formed(secret: &Secret) -> Result<(), ProviderError> {
    match secret.expose().split_once(':') {
        Some((id, s)) if !id.is_empty() && !s.is_empty() => Ok(()),
        _ => Err(fail(
            "a Higgsfield API key is the whole id:secret string the dashboard shows",
        )),
    }
}

fn usage_page(
    http: &dyn Transport,
    secret: &Secret,
    from: Date,
    to: Date,
    limit: u32,
    page: usize,
) -> Result<UsagePage, ProviderError> {
    well_formed(secret)?;
    let url = format!(
        "{URL}?start={from}&end={to}&timeframe=day&expand=auth_method&limit={limit}&page={page}"
    );
    let req = Request::get(url).secret_header("Authorization", Secret::with_prefix("Key ", secret));
    call_json::<UsagePage>(http, &req, secret).map_err(|e| e.into_provider(secret))
}

/// Pages from page 1 until a page holds no buckets, refusing a page that repeats any before it.
// The page base and the units of `total` are unverified against a real account, so neither
// decides when to stop.
fn usage(
    http: &dyn Transport,
    secret: &Secret,
    from: Date,
    to: Date,
    mut each: impl FnMut(UsagePage) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    let mut seen = std::collections::HashSet::new();
    for page in 1..=MAX_PAGES {
        let p = usage_page(http, secret, from, to, LIMIT, page)?;
        if p.time_series.is_empty() {
            return Ok(());
        }
        if !seen.insert(fingerprint(&p.time_series)) {
            return Err(fail("Higgsfield returned the same usage page twice"));
        }
        each(p)?;
    }
    Err(fail(format!("more than {MAX_PAGES} pages; stopped")))
}

impl Higgsfield {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
    ) -> Result<MonthCache, ProviderError> {
        let (start, end) = window(month, (self.today)())?;
        let mut cache = MonthCache::new("higgsfield", month, FigureKind::Estimated, Breakdown::Key);
        usage(http, &credential.secret, start, end, |page| {
            for bucket in page.time_series {
                let day = day_of(&bucket.bucket)?;
                // Bucket alignment may reach past the window; only the window's days are kept.
                if day < start || day >= end {
                    continue;
                }
                for row in bucket.results {
                    if let Some(c) = &row.currency {
                        usd(c)?;
                    }
                    cache.add(day, &key_id(row.auth_method.as_ref()), row.cost.0);
                }
            }
            Ok(())
        })?;
        Ok(cache)
    }
}

impl Provider for Higgsfield {
    fn platform(&self) -> &'static str {
        "higgsfield"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        let today = (self.today)();
        let yesterday = Date::from_days(today.to_days() - 1);
        check_outcome(
            usage_page(http, credential, yesterday, today, 1, 1)
                .map(|_| "read yesterday's model usage".to_string()),
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

    const KEY: &str = "fk-hf-id-0001:fkhfsecretpart00000000";
    const PAGE1: &str = include_str!("../../fixtures/higgsfield/usage_page1.json");
    const PAGE2: &str = include_str!("../../fixtures/higgsfield/usage_page2.json");
    const ERR_401: &str = include_str!("../../fixtures/higgsfield/error_401.json");
    const EMPTY: &str = r#"{"time_series":[],"total":3,"page":3,"limit":2}"#;

    fn provider() -> Higgsfield {
        Higgsfield {
            today: || Date::parse("2026-11-05").unwrap(),
        }
    }

    fn fetch(mock: &MockSeq, month: &str, key: &str) -> Result<MonthCache, ProviderError> {
        provider().fetch_month(
            &guarded(mock),
            &[cred("render", key)],
            Month::parse(month).unwrap(),
            &Config::default(),
        )
    }

    #[test]
    fn pages_and_sums_cost_per_key_id() {
        let mock = MockSeq::new(vec![
            ("/models/usage?", 200, PAGE1.into()),
            ("/models/usage?", 200, PAGE2.into()),
            ("/models/usage?", 200, EMPTY.into()),
        ]);
        let c = fetch(&mock, "2026-10", KEY).unwrap();
        let sent = mock.sent();
        assert_eq!(sent.len(), 3, "paging ends on an empty page, not on total");
        assert_eq!(
            sent[0].url,
            "https://api.higgsfield.ai/models/usage?start=2026-10-01&end=2026-11-01&timeframe=day&expand=auth_method&limit=100&page=1"
        );
        assert!(sent[1].url.ends_with("&page=2"), "{}", sent[1].url);
        assert!(sent[2].url.ends_with("&page=3"), "{}", sent[2].url);
        assert_eq!(
            sent[0].header("Authorization"),
            Some(format!("Key {KEY}").as_str())
        );
        assert_eq!(c.kind, FigureKind::Estimated);
        assert_eq!(c.breakdown, Breakdown::Key);
        assert_eq!(c.daily[&d("2026-10-02")]["fk-key-id-0001"], dec("1.44"));
        assert_eq!(c.daily[&d("2026-10-02")]["fk-key-id-0002"], dec("0.188"));
        assert_eq!(
            c.daily[&d("2026-10-20")]["unattributed (render)"],
            dec("0.094")
        );
        assert!(!c.daily.contains_key(&d("2026-10-03")));
        assert_eq!(total(&c), dec("1.722"));
    }

    #[test]
    fn a_repeated_page_is_refused_and_pages_are_capped() {
        // A page past the end that echoes the last one would otherwise be counted again.
        let mock = MockSeq::new(vec![
            ("/models/usage?", 200, PAGE1.into()),
            ("/models/usage?", 200, PAGE1.into()),
        ]);
        let e = fetch(&mock, "2026-10", KEY).unwrap_err();
        assert!(
            e.message.as_str().contains("same usage page twice"),
            "{}",
            e.message
        );
        let pages: Vec<_> = (0..=MAX_PAGES)
            .map(|i| {
                (
                    "/models/usage?",
                    200,
                    PAGE2.replace("0.0940", &format!("{i}")),
                )
            })
            .collect();
        let mock = MockSeq::new(pages);
        let e = fetch(&mock, "2026-10", KEY).unwrap_err();
        assert!(
            e.message.as_str().contains("pages; stopped"),
            "{}",
            e.message
        );
        assert_eq!(mock.sent().len(), MAX_PAGES);
    }

    #[test]
    fn a_page_repeating_any_earlier_page_is_refused() {
        // Page 3 echoes page 1, which a check against only the page before would count again.
        let mock = MockSeq::new(vec![
            ("/models/usage?", 200, PAGE1.into()),
            ("/models/usage?", 200, PAGE2.into()),
            ("/models/usage?", 200, PAGE1.into()),
            ("/models/usage?", 200, EMPTY.into()),
        ]);
        let e = fetch(&mock, "2026-10", KEY).unwrap_err();
        assert!(
            e.message.as_str().contains("same usage page twice"),
            "{}",
            e.message
        );
        assert_eq!(mock.sent().len(), 3);
    }

    #[test]
    fn buckets_outside_the_window_and_other_currencies() {
        let outside = PAGE2.replace("2026-10-20", "2026-11-01");
        let mock = MockSeq::new(vec![
            ("/models/usage?", 200, outside),
            ("/models/usage?", 200, EMPTY.into()),
        ]);
        assert!(fetch(&mock, "2026-10", KEY).unwrap().daily.is_empty());
        let eur = PAGE2.replace("\"USD\"", "\"EUR\"");
        let mock = MockSeq::new(vec![("/models/usage?", 200, eur)]);
        let e = fetch(&mock, "2026-10", KEY).unwrap_err();
        assert!(e.message.as_str().contains("USD only"), "{}", e.message);
    }

    #[test]
    fn half_a_key_is_refused_before_sending() {
        let mock = MockSeq::new(vec![]);
        for bad in ["fkhfsecretpart00000000", "fk-id:", ":fksecret"] {
            let e = fetch(&mock, "2026-10", bad).unwrap_err();
            assert!(e.message.as_str().contains("id:secret"), "{}", e.message);
        }
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn unauthorized_body_with_key_is_scrubbed() {
        let mock = MockSeq::new(vec![("/models/usage?", 401, ERR_401.replace("{KEY}", KEY))]);
        let e = fetch(&mock, "2026-10", KEY).unwrap_err();
        assert_no_key(&e, KEY);
        assert_no_key(&e, "fkhfsecretpart00000000");
        assert!(e.message.as_str().contains("HTTP 401"), "{}", e.message);
    }

    #[test]
    fn redirect_is_refused_and_check_is_cheap() {
        let mock = MockSeq::new(vec![("/models/usage?", 302, String::new())]);
        let e = fetch(&mock, "2026-10", KEY).unwrap_err();
        assert!(
            e.message.as_str().contains("redirect refused"),
            "{}",
            e.message
        );
        let ok = MockSeq::new(vec![(
            "/models/usage?",
            200,
            r#"{"time_series":[],"total":0,"page":1,"limit":1}"#.into(),
        )]);
        let out = provider().check(&guarded(&ok), &Secret::new(KEY.into()), &Config::default());
        assert!(matches!(out, CheckOutcome::Ok(_)), "{out:?}");
        assert!(ok.sent()[0].url.contains("start=2026-11-04&end=2026-11-05"));
        assert!(ok.sent()[0].url.contains("&limit=1&page=1"));
    }

    #[test]
    fn check_sends_one_request_whatever_the_total() {
        // A total beyond one page once made the check page through the whole range.
        let ok = MockSeq::new(vec![
            (
                "/models/usage?",
                200,
                PAGE2.replace("\"total\": 3", "\"total\": 50"),
            ),
            ("/models/usage?", 200, PAGE2.into()),
        ]);
        let out = provider().check(&guarded(&ok), &Secret::new(KEY.into()), &Config::default());
        assert!(matches!(out, CheckOutcome::Ok(_)), "{out:?}");
        assert_eq!(ok.sent().len(), 1);
    }
}
