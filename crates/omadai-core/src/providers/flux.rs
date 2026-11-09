//! Flux (Black Forest Labs): no usage API, so dollars are derived from drops in the credit balance.

use super::{Dec, call_json, check_outcome, derived, fail, fetch_each_balance, window};
use crate::cache::{BalanceHistory, BalancePoint, Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Month, now_rfc3339};
use crate::http::{Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use rust_decimal::Decimal;
use serde::Deserialize;

const URL: &str = "https://api.bfl.ai/v1/credits";

pub struct Flux {
    /// An RFC 3339 UTC timestamp; the day it starts with is "today".
    pub now: fn() -> String,
}

impl Default for Flux {
    fn default() -> Self {
        Flux { now: now_rfc3339 }
    }
}

#[derive(Deserialize)]
struct Credits {
    credits: Dec,
}

/// One credit is $0.01 (BFL docs).
fn dollars(credits: Decimal) -> Decimal {
    super::cents_to_dollars(credits)
}

/// A rise is a top-up, so it adds no spend.
fn spent(prev: &BalancePoint, cur: &BalancePoint) -> Result<Decimal, ProviderError> {
    Ok(dollars((prev.balance - cur.balance).max(Decimal::ZERO)))
}

fn credits(http: &dyn Transport, secret: &Secret) -> Result<Decimal, ProviderError> {
    let req = Request::get(URL).secret_header("x-key", Secret::with_prefix("", secret));
    call_json::<Credits>(http, &req, secret)
        .map(|c| c.credits.0)
        .map_err(|e| e.into_provider(secret))
}

impl Flux {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
        history: &mut BalanceHistory,
    ) -> Result<MonthCache, ProviderError> {
        let now = (self.now)();
        let today = super::day_of(&now)?;
        window(month, today)?;
        let balance = credits(http, &credential.secret)?;
        let series = &credential.series;
        derived::record(history, (series, &credential.label), now, balance, None);
        let mut cache = MonthCache::new("flux", month, FigureKind::Derived, Breakdown::Key);
        cache.names.insert(series.clone(), credential.label.clone());
        cache.credits_remaining = Some(dollars(balance));
        Ok(cache)
    }
}

impl Provider for Flux {
    fn platform(&self) -> &'static str {
        "flux"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        check_outcome(
            credits(http, credential).map(|c| format!("read the credit balance ({c} credits)")),
        )
    }

    /// Refused, so a month derived from no history can never overwrite one with zeros.
    fn fetch_month(
        &self,
        _http: &dyn Transport,
        _credentials: &[Credential],
        _month: Month,
        _config: &Config,
    ) -> Result<MonthCache, ProviderError> {
        Err(fail(
            "Flux spend is derived from its balance history, so it is fetched only with that history",
        ))
    }

    fn derived(&self) -> bool {
        true
    }

    fn fetch_derived(
        &self,
        http: &dyn Transport,
        credentials: &[Credential],
        month: Month,
        _config: &Config,
        history: &mut BalanceHistory,
    ) -> Result<MonthCache, ProviderError> {
        let mut cache = fetch_each_balance(credentials, |c| self.one(http, c, month, history))?;
        // Spend comes from every series, so a removed key's keeps; credits only from the active keys.
        let today = super::day_of(&(self.now)())?;
        derived::add_every_series(&mut cache, history, today, spent)?;
        Ok(cache)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::cache::CACHE_VERSION;
    use crate::http::mock::MockSeq;
    use std::collections::BTreeMap;

    const KEY: &str = "fk-bfl-TESTKEY0123456789abcdef";
    const ERR_401: &str = include_str!("../../fixtures/flux/error_401.json");

    thread_local! {
        /// Each test runs on its own thread, so each gets its own clock.
        static CLOCK: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    fn body(credits: &str) -> String {
        format!(r#"{{"credits": {credits}}}"#)
    }

    fn empty() -> BalanceHistory {
        BalanceHistory {
            version: CACHE_VERSION,
            platform: "flux".into(),
            points: Vec::new(),
            labels: BTreeMap::new(),
        }
    }

    /// One fetch at `at` reading `credits`, extending `history`.
    fn fetch_at(history: &mut BalanceHistory, at: &str, month: &str, credits: &str) -> MonthCache {
        fetch_as(history, at, month, credits, cred("render", KEY))
    }

    /// As `fetch_at`, with `credential` as the only configured key.
    fn fetch_as(
        history: &mut BalanceHistory,
        at: &str,
        month: &str,
        credits: &str,
        credential: Credential,
    ) -> MonthCache {
        let mock = MockSeq::new(vec![("/v1/credits", 200, body(credits))]);
        CLOCK.with(|c| *c.borrow_mut() = at.to_string());
        let p = Flux {
            now: || CLOCK.with(|c| c.borrow().clone()),
        };
        let c = p
            .fetch_derived(
                &guarded(&mock),
                &[credential],
                Month::parse(month).unwrap(),
                &Config::default(),
                history,
            )
            .unwrap();
        let sent = mock.sent();
        assert_eq!(sent[0].url, URL);
        assert_eq!(sent[0].header("x-key"), Some(KEY));
        c
    }

    #[test]
    fn first_fetch_records_a_point_and_derives_no_spend() {
        let mut h = empty();
        let c = fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "1000");
        assert_eq!(c.kind, FigureKind::Derived);
        assert!(c.daily.is_empty(), "no history yet means no spend: {c:?}");
        assert_eq!(c.credits_remaining, Some(dec("10")));
        assert_eq!(h.points.len(), 1);
        assert_eq!(h.points[0].series, "series-render");
        assert_eq!(c.names["series-render"], "render");
        assert_eq!(h.points[0].balance, dec("1000"));
    }

    #[test]
    fn drops_are_spend_and_rises_are_top_ups() {
        let mut h = empty();
        fetch_at(&mut h, "2026-10-30T09:00:00Z", "2026-10", "1500");
        // From the last October reading: 1500 -> 1200 is $3 on November 2.
        fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "1200");
        // Same day: 1200 -> 1150 adds $0.50 to November 2.
        fetch_at(&mut h, "2026-11-02T15:00:00Z", "2026-11", "1150");
        // A top-up: 1150 -> 5150 adds nothing.
        fetch_at(&mut h, "2026-11-03T09:00:00Z", "2026-11", "5150");
        // After the top-up: 5150 -> 5000.5 is $1.495 on November 4.
        let c = fetch_at(&mut h, "2026-11-04T09:00:00Z", "2026-11", "5000.5");
        assert_eq!(c.daily[&d("2026-11-02")]["series-render"], dec("3.5"));
        assert!(
            !c.daily.contains_key(&d("2026-11-03")),
            "a top-up is not spend"
        );
        assert_eq!(c.daily[&d("2026-11-04")]["series-render"], dec("1.495"));
        assert_eq!(total(&c), dec("4.995"));
        assert_eq!(c.credits_remaining, Some(dec("50.005")));
        assert_eq!(h.points.len(), 5);
    }

    #[test]
    fn a_month_with_one_point_takes_its_spend_from_the_point_before() {
        let mut h = empty();
        fetch_at(&mut h, "2026-10-30T09:00:00Z", "2026-10", "800");
        let c = fetch_at(&mut h, "2026-11-05T09:00:00Z", "2026-11", "700");
        assert_eq!(total(&c), dec("1"));
        assert_eq!(c.daily[&d("2026-11-05")]["series-render"], dec("1"));
        // The closed month keeps only its own drops: one point, nothing before it.
        let oct = fetch_at(&mut h, "2026-11-05T09:00:00Z", "2026-10", "700");
        assert!(oct.daily.is_empty(), "{oct:?}");
    }

    #[test]
    fn a_removed_series_keeps_its_spend_and_never_meets_the_next() {
        let mut h = empty();
        fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "1000");
        fetch_at(&mut h, "2026-11-03T09:00:00Z", "2026-11", "700");
        // Removed, then added again under the same label: a fresh series reading 500.
        let again = Credential {
            label: "render".into(),
            series: "series-render-again".into(),
            secret: Secret::new(KEY.into()),
        };
        let c = fetch_as(&mut h, "2026-11-04T09:00:00Z", "2026-11", "500", again);
        assert_eq!(c.daily[&d("2026-11-03")]["series-render"], dec("3"));
        assert!(
            !c.daily.contains_key(&d("2026-11-04")),
            "a delta across two series: {c:?}"
        );
        assert_eq!(total(&c), dec("3"), "{c:?}");
        assert_eq!(c.credits_remaining, Some(dec("5")), "the active key's only");
        assert_eq!(c.names["series-render"], "render (removed)");
        assert_eq!(c.names["series-render-again"], "render");
    }

    #[test]
    fn two_credentials_keep_their_series_apart_and_their_credits_add_up() {
        let mock = MockSeq::new(vec![
            ("/v1/credits", 200, body("1000")),
            ("/v1/credits", 200, body("250")),
        ]);
        let mut h = empty();
        let c = Flux::default()
            .fetch_derived(
                &guarded(&mock),
                &[
                    cred("render", KEY),
                    cred("draft", "fk-bfl-SECONDKEY0123456789"),
                ],
                Month::current_utc(),
                &Config::default(),
                &mut h,
            )
            .unwrap();
        assert_eq!(c.credits_remaining, Some(dec("12.5")));
        let series: Vec<_> = h.points.iter().map(|p| p.series.as_str()).collect();
        assert_eq!(series, ["series-render", "series-draft"]);
        assert_eq!(c.names["series-draft"], "draft");
    }

    #[test]
    fn fetch_without_history_is_refused() {
        let mock = MockSeq::new(vec![]);
        let e = Flux::default()
            .fetch_month(
                &guarded(&mock),
                &[cred("render", KEY)],
                Month::current_utc(),
                &Config::default(),
            )
            .unwrap_err();
        assert!(
            e.message.as_str().contains("balance history"),
            "{}",
            e.message
        );
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn unauthorized_body_with_key_is_scrubbed_and_records_nothing() {
        let mock = MockSeq::new(vec![("/v1/credits", 401, ERR_401.replace("{KEY}", KEY))]);
        let mut h = empty();
        let e = Flux::default()
            .fetch_derived(
                &guarded(&mock),
                &[cred("render", KEY)],
                Month::current_utc(),
                &Config::default(),
                &mut h,
            )
            .unwrap_err();
        assert_no_key(&e, KEY);
        assert!(e.message.as_str().contains("HTTP 401"), "{}", e.message);
        assert!(h.points.is_empty());
    }

    #[test]
    fn check_reads_the_balance() {
        let ok = MockSeq::new(vec![("/v1/credits", 200, body("42"))]);
        let out =
            Flux::default().check(&guarded(&ok), &Secret::new(KEY.into()), &Config::default());
        assert_eq!(
            out,
            CheckOutcome::Ok("read the credit balance (42 credits)".into())
        );
    }
}
