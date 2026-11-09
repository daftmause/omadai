//! xAI (Grok API): per-key usage dollars estimated by the platform, and the month's prepaid
//! ledger settlement as the billed total once it posts.

use super::{
    Dec, UNATTRIBUTED, bearer, call_json, cents_to_dollars, check_outcome, day_of, fail,
    fetch_each, requirement, window,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Date, Month};
use crate::http::{Request, Transport};
use crate::provider::{Cause, CheckOutcome, Credential, Provider, ProviderError};
use crate::scrub::plain;
use crate::secret::Secret;
use rust_decimal::Decimal;
use serde::Deserialize;

const BASE: &str = "https://management-api.x.ai/v1/billing/teams";

pub struct Xai {
    pub today: fn() -> Date,
}

impl Default for Xai {
    fn default() -> Self {
        Xai {
            today: Date::today_utc,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    #[serde(default)]
    time_series: Vec<Series>,
    #[serde(default)]
    limit_reached: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Series {
    #[serde(default)]
    group: Vec<String>,
    #[serde(default)]
    group_labels: Vec<String>,
    #[serde(default)]
    data_points: Vec<Point>,
}

#[derive(Deserialize)]
struct Point {
    timestamp: String,
    #[serde(default)]
    values: Vec<Dec>,
}

#[derive(Deserialize)]
struct Balance {
    #[serde(default)]
    changes: Vec<Change>,
    #[serde(default)]
    total: Option<Cents>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Change {
    #[serde(default)]
    change_origin: String,
    #[serde(default)]
    amount: Option<Cents>,
    #[serde(default)]
    spend_bp_key_year: Option<i32>,
    #[serde(default)]
    spend_bp_key_month: Option<u8>,
    #[serde(default)]
    create_time: Option<String>,
}

#[derive(Deserialize)]
struct Cents {
    val: Dec,
}

/// The team ID goes into the URL path, so only the characters a UUID uses are accepted.
pub fn valid_team_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn team_id(config: &Config) -> Result<&str, ProviderError> {
    let Some(x) = &config.xai else {
        return Err(requirement(
            "xAI needs your team ID: add it to config.toml as [xai] team_id = \"...\" \
             (xAI Console > Settings > Team)"
                .into(),
        ));
    };
    let id = x.team_id.as_str();
    if !valid_team_id(id) {
        return Err(fail("the xAI team_id in config.toml is not a team ID"));
    }
    Ok(id)
}

/// The usage request body; `endTime` is exclusive.
fn usage_body(from: Date, to: Date) -> String {
    serde_json::json!({
        "analyticsRequest": {
            "timeRange": {
                "startTime": format!("{from} 00:00:00"),
                "endTime": format!("{to} 00:00:00"),
                "timezone": "Etc/GMT"
            },
            "timeUnit": "TIME_UNIT_DAY",
            "values": [{"name": "usd", "aggregation": "AGGREGATION_SUM"}],
            "groupBy": ["api_key_id"],
            "filters": []
        }
    })
    .to_string()
}

fn usage(
    http: &dyn Transport,
    secret: &Secret,
    team: &str,
    from: Date,
    to: Date,
) -> Result<Usage, ProviderError> {
    let u = usage_any(http, secret, team, from, to)?;
    if u.limit_reached {
        return Err(fail(
            "xAI returned a partial usage result (limitReached); figures not stored",
        ));
    }
    Ok(u)
}

/// As `usage`, but a partial result comes back with `limit_reached` set rather than as an error.
fn usage_any(
    http: &dyn Transport,
    secret: &Secret,
    team: &str,
    from: Date,
    to: Date,
) -> Result<Usage, ProviderError> {
    let req = Request::post(
        format!("{BASE}/{team}/usage"),
        Secret::new(usage_body(from, to)),
    )
    .header("content-type", "application/json")
    .secret_header("Authorization", bearer(secret));
    call_json(http, &req, secret).map_err(|e| e.into_provider(secret))
}

fn balance(http: &dyn Transport, secret: &Secret, team: &str) -> Result<Balance, ProviderError> {
    let req = Request::get(format!("{BASE}/{team}/prepaid/balance"))
        .secret_header("Authorization", bearer(secret));
    call_json(http, &req, secret).map_err(|e| e.into_provider(secret))
}

/// The SPEND changes posted for `month`, in dollars, or `None` before the settlement posts.
fn settlement(b: &Balance, month: Month) -> Option<Decimal> {
    let spends: Vec<Decimal> = b
        .changes
        .iter()
        .filter(|c| {
            c.change_origin == "SPEND"
                && c.spend_bp_key_year == Some(month.year)
                && c.spend_bp_key_month == Some(month.month)
        })
        .filter_map(|c| c.amount.as_ref().map(|a| a.val.0))
        .collect();
    (!spends.is_empty()).then(|| cents_to_dollars(spends.into_iter().sum()))
}

/// The first day whose usage the ledger has not yet settled, or `None` when nothing can be owed.
fn unsettled_from(b: &Balance) -> Option<Date> {
    let settled = b
        .changes
        .iter()
        .filter(|c| c.change_origin == "SPEND")
        .filter_map(|c| Month::new(c.spend_bp_key_year?, c.spend_bp_key_month?))
        .max();
    if let Some(last) = settled {
        return Some(last.succ().first_day());
    }
    // With no settlement yet, usage can only have drawn on credit from the first purchase or other credit onwards.
    b.changes
        .iter()
        .filter(|c| {
            c.change_origin == "PURCHASE"
                || c.amount
                    .as_ref()
                    .is_some_and(|a| a.val.0.is_sign_negative())
        })
        .filter_map(|c| c.create_time.as_deref().and_then(|t| day_of(t).ok()))
        .min()
        .map(|d| d.month().first_day())
}

/// Why the unsettled-usage call failed, in omadai's words, so no response text reaches the warning.
fn unsettled_reason(e: &ProviderError) -> &'static str {
    match e.cause {
        Cause::Status(_) => "xAI refused the usage request",
        Cause::Redirect(_) => "xAI answered the usage request with a redirect, which was refused",
        Cause::Transport => "could not reach xAI for the usage request",
        _ => "xAI's usage result could not be read",
    }
}

fn usage_sum(u: &Usage) -> Decimal {
    u.time_series
        .iter()
        .flat_map(|s| &s.data_points)
        .filter_map(|p| p.values.first().map(|Dec(v)| *v))
        .sum()
}

impl Xai {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
        config: &Config,
    ) -> Result<MonthCache, ProviderError> {
        let team = team_id(config)?;
        let secret = &credential.secret;
        let today = (self.today)();
        let (start, end) = window(month, today)?;
        let mut cache = MonthCache::new("xai", month, FigureKind::Estimated, Breakdown::Key);
        let used = usage(http, secret, team, start, end)?;
        let window_sum = usage_sum(&used);
        for series in used.time_series {
            let id = series
                .group
                .first()
                .filter(|g| !g.is_empty())
                .cloned()
                .unwrap_or_else(|| UNATTRIBUTED.into());
            if let Some(label) = series.group_labels.first().filter(|l| !l.is_empty())
                && id != UNATTRIBUTED
            {
                // Already masked by xAI as `<name> (<key prefix>...<last4>)`.
                cache.names.insert(id.clone(), label.clone());
            }
            for point in series.data_points {
                let Some(Dec(usd)) = point.values.first().copied() else {
                    continue;
                };
                // Data points are dense, one per day; zero days are left out of the cache.
                if usd.is_zero() {
                    continue;
                }
                cache.add(day_of(&point.timestamp)?, &id, usd);
            }
        }
        let b = balance(http, secret, team)?;
        // xAI records credit as a negative balance, so the remaining credit is its negation.
        let ledger = b.total.as_ref().map(|t| -cents_to_dollars(t.val.0));
        let through = today.succ();
        // The ledger deducts a month's spend only once it settles, so unsettled usage is taken off.
        cache.credits_remaining = match (ledger, unsettled_from(&b)) {
            (None, _) => None,
            (Some(l), Some(from)) if from >= through => Some(l),
            (Some(l), Some(from)) if (from, through) == (start, end) => Some(l - window_sum),
            // A failed extra call costs only the credits figure, never the month's own figures.
            (Some(l), Some(from)) => {
                let credits: Result<Decimal, String> =
                    match usage_any(http, secret, team, from, through) {
                        Ok(u) if !u.limit_reached => Ok(l - usage_sum(&u)),
                        Ok(_) => {
                            Err("xAI returned a partial usage result (limitReached)".to_string())
                        }
                        Err(e) => Err(match e.cause {
                            Cause::Status(n) => format!("{} (HTTP {n})", unsettled_reason(&e)),
                            _ => unsettled_reason(&e).to_string(),
                        }),
                    };
                match credits {
                    Ok(c) => Some(c),
                    Err(why) => {
                        cache.credits_warnings.push(plain(format!(
                            "key {:?}: credits left could not be computed: {why}",
                            credential.label
                        )));
                        None
                    }
                }
            }
            (Some(l), None) => Some(l),
        };
        if month < today.month()
            && let Some(billed) = settlement(&b, month)
        {
            cache.kind = FigureKind::Billed;
            cache.billed_total = Some(billed);
        }
        Ok(cache)
    }
}

impl Provider for Xai {
    fn platform(&self) -> &'static str {
        "xai"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, config: &Config) -> CheckOutcome {
        let today = (self.today)();
        check_outcome(team_id(config).and_then(|team| {
            usage(http, credential, team, today, today.succ())
                .map(|u| format!("read today's team usage ({} keys)", u.time_series.len()))
        }))
    }

    fn fetch_month(
        &self,
        http: &dyn Transport,
        credentials: &[Credential],
        month: Month,
        config: &Config,
    ) -> Result<MonthCache, ProviderError> {
        fetch_each(credentials, |c| self.one(http, c, month, config))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::config::XaiConfig;
    use crate::http::Method;
    use crate::http::mock::MockSeq;

    const KEY: &str = "fk-xai-mgmt-TESTKEY11223344";
    const TEAM: &str = "00000000-fk00-4000-8000-000000000001";
    const USAGE_OCT: &str = include_str!("../../fixtures/xai/usage_2026-10.json");
    const USAGE_NOV: &str = include_str!("../../fixtures/xai/usage_2026-11.json");
    const BALANCE: &str = include_str!("../../fixtures/xai/prepaid_balance.json");
    const ERR_401: &str = include_str!("../../fixtures/xai/error_401.json");

    fn config() -> Config {
        Config {
            xai: Some(XaiConfig {
                team_id: TEAM.into(),
            }),
            ..Config::default()
        }
    }

    fn provider() -> Xai {
        Xai {
            today: || Date::parse("2026-11-05").unwrap(),
        }
    }

    fn fetch(mock: &MockSeq, month: &str, config: &Config) -> Result<MonthCache, ProviderError> {
        provider().fetch_month(
            &guarded(mock),
            &[cred("team", KEY)],
            Month::parse(month).unwrap(),
            config,
        )
    }

    #[test]
    fn running_month_is_estimated_per_key() {
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_NOV.into()),
            ("/prepaid/balance", 200, BALANCE.into()),
        ]);
        let c = fetch(&mock, "2026-11", &config()).unwrap();
        assert_eq!(c.kind, FigureKind::Estimated);
        assert_eq!(c.billed_total, None, "a running month has no settlement");
        assert_eq!(
            c.daily[&d("2026-11-02")]["fk-uuid-key-one"],
            dec("0.75973725")
        );
        assert_eq!(c.daily.len(), 2, "zero data points are skipped");
        assert_eq!(total(&c), dec("1.25973725"));
        assert_eq!(c.names["fk-uuid-key-one"], "Render (fk-...ab12)");
        assert_eq!(
            c.credits_remaining,
            Some(dec("23.74026275")),
            "the ledger's 25 less November's unsettled usage, read from the same call"
        );
        let sent = mock.sent();
        assert_eq!(
            sent.len(),
            2,
            "the running month alone is unsettled: no extra call"
        );
        assert_eq!(sent[0].method, Method::Post);
        assert_eq!(
            sent[0].url,
            format!("https://management-api.x.ai/v1/billing/teams/{TEAM}/usage")
        );
        assert_eq!(sent[0].header("content-type"), Some("application/json"));
        assert_eq!(
            sent[0].header("Authorization"),
            Some(format!("Bearer {KEY}").as_str())
        );
        let body: serde_json::Value =
            serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        let want: serde_json::Value = serde_json::from_str(
            r#"{"analyticsRequest": {"timeRange": {"startTime": "2026-11-01 00:00:00", "endTime": "2026-11-06 00:00:00", "timezone": "Etc/GMT"}, "timeUnit": "TIME_UNIT_DAY", "values": [{"name": "usd", "aggregation": "AGGREGATION_SUM"}], "groupBy": ["api_key_id"], "filters": []}}"#,
        )
        .unwrap();
        assert_eq!(body, want);
        assert_eq!(sent[1].method, Method::Get);
    }

    #[test]
    fn closed_settled_month_is_billed_with_estimated_breakdown() {
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_OCT.into()),
            ("/prepaid/balance", 200, BALANCE.into()),
            ("/usage", 200, USAGE_NOV.into()),
        ]);
        let c = fetch(&mock, "2026-10", &config()).unwrap();
        assert_eq!(c.kind, FigureKind::Billed);
        assert_eq!(c.credits_remaining, Some(dec("23.74026275")));
        assert_eq!(
            c.billed_total,
            Some(dec("10")),
            "1000 cents of SPEND for 2026-10"
        );
        assert_eq!(
            total(&c),
            dec("10.23"),
            "per-key estimates are kept as they are"
        );
        assert!(
            mock.sent()[0]
                .body
                .as_deref()
                .unwrap()
                .contains(r#""endTime":"2026-11-01 00:00:00""#)
        );
    }

    #[test]
    fn closed_month_before_settlement_stays_estimated() {
        let unsettled = BALANCE.replace("\"spendBpKeyMonth\": 10", "\"spendBpKeyMonth\": 9");
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_OCT.into()),
            ("/prepaid/balance", 200, unsettled),
            ("/usage", 200, USAGE_OCT.into()),
        ]);
        let c = fetch(&mock, "2026-10", &config()).unwrap();
        assert_eq!(c.kind, FigureKind::Estimated);
        assert_eq!(c.billed_total, None);
    }

    fn time_range(mock: &MockSeq, i: usize) -> (String, String) {
        let body: serde_json::Value =
            serde_json::from_str(mock.sent()[i].body.as_deref().unwrap()).unwrap();
        let r = &body["analyticsRequest"]["timeRange"];
        (
            r["startTime"].as_str().unwrap().into(),
            r["endTime"].as_str().unwrap().into(),
        )
    }

    #[test]
    fn an_unsettled_earlier_month_is_read_in_one_extra_call_and_taken_off() {
        // Settled through September, so October and November are both still owed.
        let unsettled = BALANCE.replace("\"spendBpKeyMonth\": 10", "\"spendBpKeyMonth\": 9");
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_NOV.into()),
            ("/prepaid/balance", 200, unsettled),
            ("/usage", 200, USAGE_OCT.into()),
        ]);
        let c = fetch(&mock, "2026-11", &config()).unwrap();
        assert_eq!(mock.sent().len(), 3);
        assert_eq!(
            time_range(&mock, 2),
            ("2026-10-01 00:00:00".into(), "2026-11-06 00:00:00".into())
        );
        assert_eq!(c.credits_remaining, Some(dec("14.77")), "25 less 10.23");
        assert_eq!(
            total(&c),
            dec("1.25973725"),
            "the month's own figures are unchanged"
        );
    }

    #[test]
    fn with_no_settlement_usage_counts_from_the_first_purchase_month() {
        let v: serde_json::Value = serde_json::from_str(BALANCE).unwrap();
        let purchases: Vec<_> = v["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["changeOrigin"] == "PURCHASE")
            .cloned()
            .collect();
        let no_spend = serde_json::json!({"changes": purchases, "total": v["total"]}).to_string();
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_NOV.into()),
            ("/prepaid/balance", 200, no_spend),
            ("/usage", 200, USAGE_OCT.into()),
        ]);
        let c = fetch(&mock, "2026-11", &config()).unwrap();
        assert_eq!(
            time_range(&mock, 2),
            ("2026-09-01 00:00:00".into(), "2026-11-06 00:00:00".into())
        );
        assert_eq!(c.credits_remaining, Some(dec("14.77")));

        let empty = r#"{"changes": [], "total": {"val": "0"}}"#;
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_NOV.into()),
            ("/prepaid/balance", 200, empty.into()),
        ]);
        let c = fetch(&mock, "2026-11", &config()).unwrap();
        assert_eq!(
            mock.sent().len(),
            2,
            "nothing bought, nothing owed against credit"
        );
        assert_eq!(c.credits_remaining, Some(dec("0")));
    }

    #[test]
    fn with_no_settlement_a_credit_of_any_origin_starts_the_unsettled_range() {
        let grant = r#"{"changes": [{"changeOrigin": "fk-GRANT", "amount": {"val": "-2500"}, "createTime": "2026-10-01T00:00:00Z"}], "total": {"val": "-2500"}}"#;
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_NOV.into()),
            ("/prepaid/balance", 200, grant.into()),
            ("/usage", 200, USAGE_OCT.into()),
        ]);
        let c = fetch(&mock, "2026-11", &config()).unwrap();
        assert_eq!(
            time_range(&mock, 2),
            ("2026-10-01 00:00:00".into(), "2026-11-06 00:00:00".into())
        );
        assert_eq!(c.credits_remaining, Some(dec("14.77")));
    }

    #[test]
    fn unsettled_usage_beyond_the_credit_goes_negative() {
        let low = BALANCE.replace("\"val\": \"-2500\"", "\"val\": \"-100\"");
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_NOV.into()),
            ("/prepaid/balance", 200, low),
        ]);
        let c = fetch(&mock, "2026-11", &config()).unwrap();
        assert_eq!(c.credits_remaining, Some(dec("-0.25973725")));
    }

    #[test]
    fn a_failed_unsettled_call_keeps_the_month_and_drops_only_the_credits() {
        let unsettled = BALANCE.replace("\"spendBpKeyMonth\": 10", "\"spendBpKeyMonth\": 9");
        let partial = USAGE_OCT.replace("\"limitReached\": false", "\"limitReached\": true");
        for (status, body, why) in [
            (200, partial, "limitReached"),
            (500, ERR_401.replace("{KEY}", KEY), "HTTP 500"),
        ] {
            let mock = MockSeq::new(vec![
                ("/usage", 200, USAGE_NOV.into()),
                ("/prepaid/balance", 200, unsettled.clone()),
                ("/usage", status, body),
            ]);
            let c = fetch(&mock, "2026-11", &config()).unwrap();
            assert_eq!(total(&c), dec("1.25973725"), "the month's figures are kept");
            assert_eq!(c.credits_remaining, None);
            assert!(
                c.warnings.is_empty(),
                "no spend is missing: {:?}",
                c.warnings
            );
            assert_eq!(c.credits_warnings.len(), 1, "{:?}", c.credits_warnings);
            let w = c.credits_warnings[0].as_str();
            assert!(
                w.starts_with("key \"team\": credits left could not be computed: "),
                "{w}"
            );
            assert!(w.contains(why), "{w}");
            assert!(!w.contains(KEY), "{w}");
        }
        // A closed month's settlement still stores when the extra call fails.
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_OCT.into()),
            ("/prepaid/balance", 200, BALANCE.into()),
            ("/usage", 503, String::new()),
        ]);
        let c = fetch(&mock, "2026-10", &config()).unwrap();
        assert_eq!(c.kind, FigureKind::Billed);
        assert_eq!(c.billed_total, Some(dec("10")));
        assert_eq!(c.credits_remaining, None);
        assert_eq!(c.credits_warnings.len(), 1);
    }

    #[test]
    fn with_two_keys_a_failed_credits_call_keeps_its_labelled_warning() {
        const KEY2: &str = "fk-xai-mgmt-TESTKEY55667788";
        let unsettled = BALANCE.replace("\"spendBpKeyMonth\": 10", "\"spendBpKeyMonth\": 9");
        let ok = (200, USAGE_OCT.to_string());
        let bad = (503, String::new());
        for (first, second, failed) in [(bad.clone(), ok.clone(), "a"), (ok, bad, "b")] {
            let mock = MockSeq::new(vec![
                ("/usage", 200, USAGE_NOV.into()),
                ("/prepaid/balance", 200, unsettled.clone()),
                ("/usage", first.0, first.1),
                ("/usage", 200, USAGE_NOV.into()),
                ("/prepaid/balance", 200, unsettled.clone()),
                ("/usage", second.0, second.1),
            ]);
            let c = provider()
                .fetch_month(
                    &guarded(&mock),
                    &[cred("a", KEY), cred("b", KEY2)],
                    Month::parse("2026-11").unwrap(),
                    &config(),
                )
                .unwrap();
            assert_eq!(mock.sent().len(), 6);
            assert_eq!(
                c.credits_remaining,
                Some(dec("14.77")),
                "the other key's figure"
            );
            assert_eq!(c.credits_warnings.len(), 1, "{:?}", c.credits_warnings);
            let w = c.credits_warnings[0].as_str();
            assert!(w.starts_with(&format!("key {failed:?}: ")), "{w}");
            assert!(w.contains("HTTP 503"), "{w}");
        }
    }

    #[test]
    fn no_ledger_total_makes_no_extra_call() {
        let v: serde_json::Value = serde_json::from_str(BALANCE).unwrap();
        let no_total = serde_json::json!({"changes": v["changes"]}).to_string();
        let mock = MockSeq::new(vec![
            ("/usage", 200, USAGE_OCT.into()),
            ("/prepaid/balance", 200, no_total),
        ]);
        let c = fetch(&mock, "2026-10", &config()).unwrap();
        assert_eq!(
            mock.sent().len(),
            2,
            "no usage call for a credits figure that cannot exist"
        );
        assert_eq!(c.credits_remaining, None);
        assert!(c.warnings.is_empty());
        assert!(c.credits_warnings.is_empty());
    }

    #[test]
    fn missing_team_id_is_a_requirement_and_sends_nothing() {
        let mock = MockSeq::new(vec![]);
        let e = fetch(&mock, "2026-11", &Config::default()).unwrap_err();
        assert!(e.requirement.unwrap().contains("team_id"));
        let out = provider().check(
            &guarded(&mock),
            &Secret::new(KEY.into()),
            &Config::default(),
        );
        assert!(matches!(out, CheckOutcome::Requirement(_)), "{out:?}");
        let bad = Config {
            xai: Some(XaiConfig {
                team_id: "../../evil".into(),
            }),
            ..Config::default()
        };
        assert!(fetch(&mock, "2026-11", &bad).is_err());
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn partial_result_is_an_error() {
        let partial = USAGE_NOV.replace("\"limitReached\": false", "\"limitReached\": true");
        let mock = MockSeq::new(vec![("/usage", 200, partial)]);
        let e = fetch(&mock, "2026-11", &config()).unwrap_err();
        assert!(e.message.as_str().contains("limitReached"), "{}", e.message);
    }

    #[test]
    fn unauthorized_body_with_key_is_scrubbed() {
        let mock = MockSeq::new(vec![("/usage", 401, ERR_401.replace("{KEY}", KEY))]);
        let e = fetch(&mock, "2026-11", &config()).unwrap_err();
        assert_no_key(&e, KEY);
        assert!(e.message.as_str().contains("HTTP 401"), "{}", e.message);
    }

    #[test]
    fn redirect_is_refused_and_check_reads_today() {
        let mock = MockSeq::new(vec![("/usage", 308, String::new())]);
        let e = fetch(&mock, "2026-11", &config()).unwrap_err();
        assert!(
            e.message.as_str().contains("redirect refused"),
            "{}",
            e.message
        );
        assert_eq!(mock.sent().len(), 1);
        let ok = MockSeq::new(vec![("/usage", 200, USAGE_NOV.into())]);
        let out = provider().check(&guarded(&ok), &Secret::new(KEY.into()), &config());
        assert!(matches!(out, CheckOutcome::Ok(_)), "{out:?}");
        let body: serde_json::Value =
            serde_json::from_str(ok.sent()[0].body.as_deref().unwrap()).unwrap();
        let range = &body["analyticsRequest"]["timeRange"];
        assert_eq!(range["startTime"], "2026-11-05 00:00:00");
        assert_eq!(range["endTime"], "2026-11-06 00:00:00");
    }
}
