//! Fish Audio: dollars derived from lifetime API spend, `cumulative_top_up − credit`, assumed USD.

use super::{Dec, bearer, call_json, check_outcome, derived, fail, fetch_each_balance, window};
use crate::cache::{BalanceHistory, BalancePoint, Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Month, now_rfc3339};
use crate::http::{Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use rust_decimal::Decimal;
use serde::Deserialize;

const URL: &str = "https://api.fish.audio/wallet/self/api-credit";

pub struct Fish {
    /// An RFC 3339 UTC timestamp; the day it starts with is "today".
    pub now: fn() -> String,
}

impl Default for Fish {
    fn default() -> Self {
        Fish { now: now_rfc3339 }
    }
}

#[derive(Deserialize)]
struct ApiCredit {
    /// The account the wallet belongs to; every key of one account reads the same wallet.
    #[serde(default)]
    user_id: Option<String>,
    credit: Dec,
    cumulative_top_up: Dec,
}

fn api_credit(http: &dyn Transport, secret: &Secret) -> Result<ApiCredit, ProviderError> {
    let req = Request::get(URL).secret_header("Authorization", bearer(secret));
    call_json(http, &req, secret).map_err(|e| e.into_provider(secret))
}

/// Lifetime API spend at a reading; top-ups raise both terms equally, so it never jumps on one.
fn lifetime(p: &BalancePoint) -> Result<Decimal, ProviderError> {
    p.cumulative_top_up
        .map(|top_up| top_up - p.balance)
        .ok_or_else(|| fail("a Fish Audio balance reading has no cumulative top-up"))
}

/// A credit rise without a matching top-up rise is a refund or grant, not negative spend.
fn spent(prev: &BalancePoint, cur: &BalancePoint) -> Result<Decimal, ProviderError> {
    Ok((lifetime(cur)? - lifetime(prev)?).max(Decimal::ZERO))
}

impl Fish {
    /// Records the wallet `credential` reads, or returns an empty month when a credential
    /// before it in `seen` already read that wallet.
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
        history: &mut BalanceHistory,
        seen: &mut Vec<String>,
    ) -> Result<MonthCache, ProviderError> {
        let now = (self.now)();
        let today = super::day_of(&now)?;
        window(month, today)?;
        let read = api_credit(http, &credential.secret)?;
        let mut cache = MonthCache::new("fish", month, FigureKind::Derived, Breakdown::Key);
        let user = read.user_id.filter(|u| !u.is_empty());
        // The row is named after the key, never the account ID; the series stays the wallet's.
        let series = match &user {
            Some(u) => format!("wallet {u}"),
            None => credential.series.clone(),
        };
        let name = credential.label.clone();
        if seen.contains(&series) {
            return Ok(cache);
        }
        seen.push(series.clone());
        let (credit, top_up) = (read.credit.0, read.cumulative_top_up.0);
        derived::record(history, (&series, &name), now, credit, Some(top_up));
        cache.names.insert(series, name);
        cache.credits_remaining = Some(credit);
        Ok(cache)
    }
}

impl Provider for Fish {
    fn platform(&self) -> &'static str {
        "fish"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        check_outcome(
            api_credit(http, credential).map(|_| "read the API credit balance".to_string()),
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
            "Fish Audio spend is derived from its balance history, so it is fetched only with that history",
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
        let mut seen = Vec::new();
        let mut cache = fetch_each_balance(credentials, |c| {
            self.one(http, c, month, history, &mut seen)
        })?;
        // Spend comes from every wallet, so a removed key's keeps; credits only from the active keys.
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

    const KEY: &str = "fk-fish-TESTKEY0123456789abcdef";
    const CREDIT: &str = include_str!("../../fixtures/fish/api_credit.json");
    const ERR_401: &str = include_str!("../../fixtures/fish/error_401.json");
    /// The series ID of the fixture's wallet.
    const WALLET: &str = "wallet fk-user-0001";

    thread_local! {
        /// Each test runs on its own thread, so each gets its own clock.
        static CLOCK: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    fn empty() -> BalanceHistory {
        BalanceHistory {
            version: CACHE_VERSION,
            platform: "fish".into(),
            points: Vec::new(),
            labels: BTreeMap::new(),
        }
    }

    fn fetch_at(
        history: &mut BalanceHistory,
        at: &str,
        month: &str,
        credit: &str,
        top_up: &str,
    ) -> MonthCache {
        fetch_as(
            history,
            at,
            month,
            ("fk-user-0001", credit, top_up),
            cred("voice", KEY),
        )
    }

    /// As `fetch_at`, with `credential` as the only configured key reading `(user_id, credit, top_up)`.
    fn fetch_as(
        history: &mut BalanceHistory,
        at: &str,
        month: &str,
        (user, credit, top_up): (&str, &str, &str),
        credential: Credential,
    ) -> MonthCache {
        let reply = CREDIT
            .replace("fk-user-0001", user)
            .replace("{CREDIT}", credit)
            .replace("{TOP_UP}", top_up);
        let mock = MockSeq::new(vec![("/wallet/self/api-credit", 200, reply)]);
        CLOCK.with(|c| *c.borrow_mut() = at.to_string());
        let p = Fish {
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
        assert_eq!(
            sent[0].header("Authorization"),
            Some(format!("Bearer {KEY}").as_str())
        );
        c
    }

    #[test]
    fn first_fetch_has_no_spend_even_with_lifetime_spend_behind_it() {
        let mut h = empty();
        // $12.50 already spent before omadai started: not a jump into this month.
        let c = fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "7.50", "20.00");
        assert_eq!(c.kind, FigureKind::Derived);
        assert!(c.daily.is_empty(), "{c:?}");
        assert_eq!(c.credits_remaining, Some(dec("7.50")));
        assert_eq!(c.names[WALLET], "voice");
        assert_eq!(h.points[0].series, WALLET);
        assert_eq!(h.points[0].cumulative_top_up, Some(dec("20.00")));
    }

    #[test]
    fn spend_is_the_change_in_lifetime_spend_across_top_ups() {
        let mut h = empty();
        fetch_at(&mut h, "2026-10-30T09:00:00Z", "2026-10", "10.00", "20.00");
        // Lifetime 10 -> 12.25: $2.25 on November 2, from the last October reading.
        fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "7.75", "20.00");
        // Same day, 12.25 -> 12.40.
        fetch_at(&mut h, "2026-11-02T15:00:00Z", "2026-11", "7.60", "20.00");
        // A $50 top-up with $0.10 spent beside it: lifetime 12.40 -> 12.50, not a $50 swing.
        let c = fetch_at(&mut h, "2026-11-03T09:00:00Z", "2026-11", "57.50", "70.00");
        assert_eq!(c.daily[&d("2026-11-02")][WALLET], dec("2.40"));
        assert_eq!(c.daily[&d("2026-11-03")][WALLET], dec("0.10"));
        assert_eq!(total(&c), dec("2.50"));
        assert_eq!(c.credits_remaining, Some(dec("57.50")));
        // October holds one point and nothing before it, so nothing was spent in it.
        let oct = fetch_at(&mut h, "2026-11-04T09:00:00Z", "2026-10", "57.50", "70.00");
        assert!(oct.daily.is_empty(), "{oct:?}");
    }

    /// One fetch at `at` for two credentials, each reading `(user_id, credit, top_up)`.
    fn fetch_two(
        history: &mut BalanceHistory,
        at: &str,
        readings: [(&str, &str, &str); 2],
    ) -> MonthCache {
        let replies = readings
            .iter()
            .map(|(user, credit, top_up)| {
                let body = CREDIT
                    .replace("fk-user-0001", user)
                    .replace("{CREDIT}", credit)
                    .replace("{TOP_UP}", top_up);
                ("/wallet/self/api-credit", 200, body)
            })
            .collect();
        let mock = MockSeq::new(replies);
        CLOCK.with(|c| *c.borrow_mut() = at.to_string());
        let p = Fish {
            now: || CLOCK.with(|c| c.borrow().clone()),
        };
        let c = p
            .fetch_derived(
                &guarded(&mock),
                &[
                    cred("voice", KEY),
                    cred("dub", "fk-fish-SECONDKEY0123456789"),
                ],
                Month::parse("2026-11").unwrap(),
                &Config::default(),
                history,
            )
            .unwrap();
        assert!(mock.unused().is_empty(), "{:?}", mock.unused());
        c
    }

    #[test]
    fn two_keys_on_one_wallet_count_its_spend_once() {
        let mut h = empty();
        let one = "fk-user-0001";
        fetch_two(
            &mut h,
            "2026-11-02T09:00:00Z",
            [(one, "10", "20"), (one, "10", "20")],
        );
        let c = fetch_two(
            &mut h,
            "2026-11-03T09:00:00Z",
            [(one, "7", "20"), (one, "7", "20")],
        );
        assert_eq!(total(&c), dec("3"), "{c:?}");
        assert_eq!(h.points.len(), 2, "one reading per wallet per fetch");
        assert_eq!(c.credits_remaining, Some(dec("7")));
        assert_eq!(c.names[WALLET], "voice", "the first key names the wallet");
        assert_eq!(c.names.len(), 1, "{:?}", c.names);
    }

    #[test]
    fn two_wallets_add_up() {
        let mut h = empty();
        let (one, two) = ("fk-user-0001", "fk-user-0002");
        fetch_two(
            &mut h,
            "2026-11-02T09:00:00Z",
            [(one, "10", "20"), (two, "5", "5")],
        );
        let c = fetch_two(
            &mut h,
            "2026-11-03T09:00:00Z",
            [(one, "7", "20"), (two, "4", "5")],
        );
        assert_eq!(total(&c), dec("4"), "{c:?}");
        assert_eq!(c.daily[&d("2026-11-03")]["wallet fk-user-0002"], dec("1"));
        assert_eq!(c.credits_remaining, Some(dec("11")));
    }

    #[test]
    fn a_removed_wallet_keeps_its_spend_and_never_meets_the_next() {
        let mut h = empty();
        fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "10", "20");
        fetch_at(&mut h, "2026-11-03T09:00:00Z", "2026-11", "7", "20");
        // Removed, then added again under the same label for another account's wallet.
        let again = Credential {
            label: "voice".into(),
            series: "series-voice-again".into(),
            secret: Secret::new(KEY.into()),
        };
        let c = fetch_as(
            &mut h,
            "2026-11-04T09:00:00Z",
            "2026-11",
            ("fk-user-0002", "5", "30"),
            again,
        );
        assert_eq!(c.daily[&d("2026-11-03")][WALLET], dec("3"));
        assert!(
            !c.daily.contains_key(&d("2026-11-04")),
            "a delta across two wallets: {c:?}"
        );
        assert_eq!(total(&c), dec("3"), "{c:?}");
        assert_eq!(
            c.credits_remaining,
            Some(dec("5")),
            "the active wallet's only"
        );
    }

    #[test]
    fn a_removed_wallet_with_an_unreadable_reading_does_not_fail_the_fetch() {
        let mut h = empty();
        fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "10", "20");
        // A retired wallet whose readings carry no top-up cannot be turned into spend.
        for at in ["2026-11-01T09:00:00Z", "2026-11-02T09:00:00Z"] {
            derived::record(&mut h, ("wallet gone", "gone"), at.into(), dec("5"), None);
        }
        let c = fetch_at(&mut h, "2026-11-03T09:00:00Z", "2026-11", "7", "20");
        assert_eq!(c.daily[&d("2026-11-03")][WALLET], dec("3"));
        assert_eq!(total(&c), dec("3"), "{c:?}");
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
    }

    #[test]
    fn a_credit_rise_without_a_top_up_is_not_negative_spend() {
        let mut h = empty();
        fetch_at(&mut h, "2026-11-02T09:00:00Z", "2026-11", "10.00", "20.00");
        // Lifetime 10 -> 5: a refund or grant, not -$5.
        fetch_at(&mut h, "2026-11-03T09:00:00Z", "2026-11", "15.00", "20.00");
        // Lifetime 5 -> 6: $1, counted from the reading after the rise.
        let c = fetch_at(&mut h, "2026-11-04T09:00:00Z", "2026-11", "14.00", "20.00");
        assert!(!c.daily.contains_key(&d("2026-11-03")), "{c:?}");
        assert_eq!(c.daily[&d("2026-11-04")][WALLET], dec("1"));
        assert_eq!(total(&c), dec("1"));
    }

    #[test]
    fn without_a_user_id_the_series_is_the_credential() {
        let reply = CREDIT
            .replace("  \"user_id\": \"fk-user-0001\",\n", "")
            .replace("{CREDIT}", "3")
            .replace("{TOP_UP}", "5");
        assert!(!reply.contains("user_id"), "{reply}");
        let mock = MockSeq::new(vec![("/wallet/self/api-credit", 200, reply)]);
        let mut h = empty();
        let c = Fish::default()
            .fetch_derived(
                &guarded(&mock),
                &[cred("voice", KEY)],
                Month::current_utc(),
                &Config::default(),
                &mut h,
            )
            .unwrap();
        assert_eq!(h.points[0].series, "series-voice");
        assert_eq!(c.names["series-voice"], "voice");
    }

    #[test]
    fn fetch_without_history_is_refused() {
        let mock = MockSeq::new(vec![]);
        let e = Fish::default()
            .fetch_month(
                &guarded(&mock),
                &[cred("voice", KEY)],
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
        let mock = MockSeq::new(vec![(
            "/wallet/self/api-credit",
            401,
            ERR_401.replace("{KEY}", KEY),
        )]);
        let mut h = empty();
        let e = Fish::default()
            .fetch_derived(
                &guarded(&mock),
                &[cred("voice", KEY)],
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
    fn check_reads_the_credit() {
        let reply = CREDIT.replace("{CREDIT}", "1").replace("{TOP_UP}", "2");
        let ok = MockSeq::new(vec![("/wallet/self/api-credit", 200, reply)]);
        let out =
            Fish::default().check(&guarded(&ok), &Secret::new(KEY.into()), &Config::default());
        assert!(matches!(out, CheckOutcome::Ok(_)), "{out:?}");
    }
}
