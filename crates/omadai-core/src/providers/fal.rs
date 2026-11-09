//! fal.ai: billed dollars per API key from the models usage endpoint, read with a BILLING-preset key.

use super::{
    CallError, Dec, UNATTRIBUTED, USER_PREFIX, call_json, check_outcome, day_of, enc, fetch_each,
    next_cursor, paged, requirement, usd, window,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Date, Month};
use crate::http::{HttpError, Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use serde::Deserialize;

const URL: &str = "https://api.fal.ai/v1/models/usage";

pub struct Fal {
    pub today: fn() -> Date,
}

impl Default for Fal {
    fn default() -> Self {
        Fal {
            today: Date::today_utc,
        }
    }
}

#[derive(Deserialize)]
struct UsagePage {
    #[serde(default)]
    time_series: Vec<Bucket>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Bucket {
    bucket: String,
    #[serde(default)]
    results: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    cost_total: Dec,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    auth_method_structured: Option<Auth>,
}

#[derive(Deserialize)]
struct Auth {
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    api_key_id: Option<String>,
    #[serde(default)]
    login_username: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    kind: String,
}

/// Only a 403 whose body says `authorization_error` means the key lacks the BILLING preset.
fn map_error(e: CallError, secret: &Secret) -> ProviderError {
    if let CallError::Http(HttpError::Status { status: 403, body }) = &e
        && serde_json::from_str::<ErrorBody>(body.as_str())
            .is_ok_and(|b| b.error.kind == "authorization_error")
    {
        return requirement(format!(
            "fal.ai spend needs a key created with the BILLING (or FULL) preset; this key may not \
             read usage. Create one in the fal.ai dashboard under API keys. fal.ai replied HTTP 403: {body}"
        ));
    }
    e.into_provider(secret)
}

fn usage(
    http: &dyn Transport,
    secret: &Secret,
    from: Date,
    to: Date,
    expand: &str,
    extra: &str,
    mut each: impl FnMut(UsagePage) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    paged(
        |cursor| {
            let mut url =
                format!("{URL}?start={from}&end={to}&timeframe=day&expand={expand}{extra}");
            if let Some(c) = cursor {
                url.push_str(&format!("&cursor={}", enc(c)));
            }
            let req = Request::get(url)
                .secret_header("Authorization", Secret::with_prefix("Key ", secret));
            call_json::<UsagePage>(http, &req, secret).map_err(|e| map_error(e, secret))
        },
        |p| next_cursor(p.has_more, p.next_cursor.as_ref()),
        &mut each,
    )
}

impl Fal {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
    ) -> Result<MonthCache, ProviderError> {
        let secret = &credential.secret;
        let (start, end) = window(month, (self.today)())?;
        let mut cache = MonthCache::new("fal", month, FigureKind::Billed, Breakdown::Key);
        usage(
            http,
            secret,
            start,
            end,
            "time_series,summary,auth_method_structured",
            "",
            |page| {
                for bucket in page.time_series {
                    let day = day_of(&bucket.bucket)?;
                    for row in bucket.results {
                        if let Some(c) = &row.currency {
                            usd(c)?;
                        }
                        let auth = row.auth_method_structured;
                        let (id, name) = match auth {
                            Some(Auth {
                                api_key_id: Some(id),
                                detail,
                                ..
                            }) => (id, detail),
                            Some(Auth {
                                login_username: Some(user),
                                detail,
                                ..
                            }) => (format!("{USER_PREFIX}{user}"), detail),
                            Some(Auth { detail, .. }) => (UNATTRIBUTED.to_string(), detail),
                            None => (UNATTRIBUTED.to_string(), None),
                        };
                        if let Some(n) = name.filter(|n| !n.is_empty())
                            && id != UNATTRIBUTED
                        {
                            cache.names.entry(id.clone()).or_insert(n);
                        }
                        cache.add(day, &id, row.cost_total.0);
                    }
                }
                Ok(())
            },
        )?;
        Ok(cache)
    }
}

impl Provider for Fal {
    fn platform(&self) -> &'static str {
        "fal"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        let today = (self.today)();
        let yesterday = Date::from_days(today.to_days() - 1);
        check_outcome(
            usage(
                http,
                credential,
                yesterday,
                today,
                "summary,auth_method_structured",
                "&limit=1",
                |_| Ok(()),
            )
            .map(|()| "read yesterday's model usage".to_string()),
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

    const KEY: &str = "fk-fal-billing-TESTKEY97531:fksecretpart0000";
    const PAGE1: &str = include_str!("../../fixtures/fal/usage_page1.json");
    const PAGE2: &str = include_str!("../../fixtures/fal/usage_page2.json");
    const ERR_403: &str = include_str!("../../fixtures/fal/error_403.json");
    const ERR_400: &str = include_str!("../../fixtures/fal/error_400.json");
    const ERR_401: &str = include_str!("../../fixtures/fal/error_401.json");

    fn provider() -> Fal {
        Fal {
            today: || Date::parse("2026-11-05").unwrap(),
        }
    }

    fn fetch(mock: &MockSeq, month: &str) -> Result<MonthCache, ProviderError> {
        provider().fetch_month(
            &guarded(mock),
            &[cred("render", KEY)],
            Month::parse(month).unwrap(),
            &Config::default(),
        )
    }

    #[test]
    fn pages_and_sums_cost_total_per_key() {
        let mock = MockSeq::new(vec![
            ("/v1/models/usage?", 200, PAGE1.into()),
            ("/v1/models/usage?", 200, PAGE2.into()),
        ]);
        let c = fetch(&mock, "2026-10").unwrap();
        let sent = mock.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0].url,
            "https://api.fal.ai/v1/models/usage?start=2026-10-01&end=2026-11-01&timeframe=day&expand=time_series,summary,auth_method_structured"
        );
        assert!(sent[1].url.ends_with("&cursor=Mg%3D%3D"), "{}", sent[1].url);
        assert_eq!(
            sent[0].header("Authorization"),
            Some(format!("Key {KEY}").as_str())
        );
        assert_eq!(c.kind, FigureKind::Billed);
        assert_eq!(
            c.daily[&d("2026-10-03")]["fk_key_a"],
            dec("0.75"),
            "cost_total, not cost_subtotal"
        );
        assert_eq!(c.daily[&d("2026-10-03")]["fk_key_b"], dec("0.04"));
        assert_eq!(c.daily[&d("2026-10-20")]["fk_key_a"], dec("1.6"));
        assert_eq!(
            c.daily[&d("2026-10-20")]["user:fk-member (render)"],
            dec("0.5")
        );
        assert_eq!(total(&c), dec("2.89"));
        assert_eq!(c.names["fk_key_a"], "Key 1");
        assert!(
            !c.daily.contains_key(&d("2026-10-10")),
            "an empty bucket adds nothing"
        );
    }

    #[test]
    fn missing_billing_preset_is_a_requirement_row() {
        let mock = MockSeq::new(vec![(
            "/v1/models/usage?",
            403,
            ERR_403.replace("{KEY}", KEY),
        )]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert_no_key(&e, KEY);
        let r = e.requirement.expect("requirement row");
        assert!(r.contains("BILLING"), "{r}");
        let check = provider().check(
            &guarded(&MockSeq::new(vec![(
                "/v1/models/usage?",
                403,
                ERR_403.into(),
            )])),
            &Secret::new(KEY.into()),
            &Config::default(),
        );
        assert!(matches!(check, CheckOutcome::Requirement(_)), "{check:?}");
    }

    #[test]
    fn validation_error_is_an_error_not_a_requirement() {
        let mock = MockSeq::new(vec![("/v1/models/usage?", 400, ERR_400.into())]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert_eq!(e.requirement, None);
        assert!(e.message.as_str().contains("HTTP 400"), "{}", e.message);
        assert!(
            e.message.as_str().contains("validation_error"),
            "{}",
            e.message
        );
        let other_403 = MockSeq::new(vec![(
            "/v1/models/usage?",
            403,
            r#"{"error":{"type":"server_error","message":"no"}}"#.into(),
        )]);
        assert_eq!(fetch(&other_403, "2026-10").unwrap_err().requirement, None);
    }

    #[test]
    fn unauthorized_body_with_key_is_scrubbed() {
        let mock = MockSeq::new(vec![(
            "/v1/models/usage?",
            401,
            ERR_401.replace("{KEY}", KEY),
        )]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert_no_key(&e, KEY);
        assert_no_key(&e, "fksecretpart0000");
        assert!(e.message.as_str().contains("[redacted]"), "{}", e.message);
    }

    #[test]
    fn redirect_is_refused_and_check_is_cheap() {
        let mock = MockSeq::new(vec![("/v1/models/usage?", 301, String::new())]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert!(
            e.message.as_str().contains("redirect refused"),
            "{}",
            e.message
        );
        assert_eq!(mock.sent().len(), 1);
        let ok = MockSeq::new(vec![("/v1/models/usage?", 200, PAGE2.into())]);
        let out = provider().check(&guarded(&ok), &Secret::new(KEY.into()), &Config::default());
        assert!(matches!(out, CheckOutcome::Ok(_)), "{out:?}");
        assert!(ok.sent()[0].url.contains("start=2026-11-04&end=2026-11-05"));
        assert!(ok.sent()[0].url.contains("&limit=1"));
    }
}
