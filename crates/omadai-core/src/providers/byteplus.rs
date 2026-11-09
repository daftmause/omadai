//! BytePlus (Seedance on ModelArk): billed dollars per model from `ListBillDetail`, signed with
//! HMAC-SHA256.

use super::{
    CallError, Dec, call_json, check_outcome, day_of, enc, fail, fetch_each, fingerprint, paged,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::Config;
use crate::date::{Month, now_rfc3339};
use crate::http::{HttpError, Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const HOST: &str = "open.byteplusapi.com";
const REGION: &str = "ap-singapore-1";
const SERVICE: &str = "billing";
const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";
const SIGNED_HEADERS: &str = "content-type;host;x-content-sha256;x-date";
/// The largest page ListBillDetail returns.
const LIMIT: u32 = 300;
/// The products whose rows are Seedance spend.
const SEEDANCE: [&str; 2] = ["ModelArk_video_generation", "ModelArk_resource_packages"];

pub struct BytePlus {
    /// An RFC 3339 UTC timestamp, used for the signature date and as "today".
    pub now: fn() -> String,
}

impl Default for BytePlus {
    fn default() -> Self {
        BytePlus { now: now_rfc3339 }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Envelope {
    #[serde(default)]
    response_metadata: Option<Metadata>,
    #[serde(default)]
    result: Option<Bill>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Metadata {
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiError {
    #[serde(default)]
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Bill {
    #[serde(default)]
    list: Vec<Row>,
    #[serde(default)]
    total: u64,
}

#[derive(Hash, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Row {
    #[serde(default)]
    product: String,
    #[serde(default)]
    config_name: String,
    #[serde(default)]
    expense_date: String,
    #[serde(default)]
    paid_amount: Option<Dec>,
    #[serde(default)]
    unpaid_amount: Option<Dec>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(rename = "OwnerID", default)]
    owner_id: Option<serde_json::Value>,
    // Read only so the repeated-page fingerprint tells apart equal charges on different resources.
    #[serde(rename = "BillDetailId", default)]
    bill_detail_id: Option<serde_json::Value>,
    #[serde(default)]
    instance_no: Option<serde_json::Value>,
    #[serde(rename = "ResourceID", default)]
    resource_id: Option<serde_json::Value>,
    #[serde(default)]
    element: Option<serde_json::Value>,
    #[serde(default)]
    count: Option<serde_json::Value>,
}

/// The AccessKey ID and Secret Access Key, stored joined as `AK:SK`.
fn split(secret: &Secret) -> Result<(&str, &str), ProviderError> {
    match secret.expose().split_once(':') {
        Some((ak, sk)) if !ak.is_empty() && !sk.is_empty() && !sk.contains(':') => Ok((ak, sk)),
        _ => Err(fail(
            "the BytePlus credential is not an AccessKey ID and Secret Access Key pair; add it again",
        )),
    }
}

/// `YYYYMMDD'T'HHMMSS'Z'` from an RFC 3339 UTC timestamp such as `now_rfc3339` writes.
fn x_date(rfc3339: &str) -> Result<String, ProviderError> {
    let x: String = rfc3339
        .chars()
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    let ok = x.len() == 16
        && x.as_bytes()[8] == b'T'
        && x.ends_with('Z')
        && x[..8]
            .bytes()
            .chain(x[9..15].bytes())
            .all(|b| b.is_ascii_digit());
    if ok {
        Ok(x)
    } else {
        Err(fail(format!("unexpected clock reading {rfc3339:?}")))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], msg: &str) -> Zeroizing<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any size");
    mac.update(msg.as_bytes());
    Zeroizing::new(mac.finalize().into_bytes().to_vec())
}

/// Sorted by name, names and values percent-encoded.
fn canonical_query(params: &[(&str, String)]) -> String {
    let mut sorted: Vec<_> = params.iter().map(|(k, v)| (enc(k), enc(v))).collect();
    sorted.sort();
    sorted
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn canonical_request(query: &str, xdate: &str, body_hash: &str) -> String {
    [
        "GET",
        "/",
        query,
        &format!("content-type:{CONTENT_TYPE}"),
        &format!("host:{HOST}"),
        &format!("x-content-sha256:{body_hash}"),
        &format!("x-date:{xdate}"),
        "",
        SIGNED_HEADERS,
        body_hash,
    ]
    .join("\n")
}

/// The credential scope and hex signature for a GET with an empty body.
fn signature(sk: &str, xdate: &str, query: &str) -> (String, String) {
    let day = &xdate[..8];
    let body_hash = hex(&Sha256::digest(b""));
    let canonical = canonical_request(query, xdate, &body_hash);
    let scope = format!("{day}/{REGION}/{SERVICE}/request");
    let to_sign = format!(
        "HMAC-SHA256\n{xdate}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let key = hmac(
        &hmac(&hmac(&hmac(sk.as_bytes(), day), REGION), SERVICE),
        "request",
    );
    (scope, hex(&hmac(&key, &to_sign)))
}

fn signed_request(
    secret: &Secret,
    xdate: &str,
    params: &[(&str, String)],
) -> Result<Request, ProviderError> {
    let (ak, sk) = split(secret)?;
    let query = canonical_query(params);
    let (scope, sig) = signature(sk, xdate, &query);
    Ok(Request::get(format!("https://{HOST}/?{query}"))
        .header("content-type", CONTENT_TYPE)
        .header("x-date", xdate)
        .header("x-content-sha256", hex(&Sha256::digest(b"")))
        .secret_header(
            "Authorization",
            Secret::new(format!(
                "HMAC-SHA256 Credential={ak}/{scope}, SignedHeaders={SIGNED_HEADERS}, Signature={sig}"
            )),
        ))
}

fn params(month: Month, limit: u32, offset: u64, ignore_zero: bool) -> Vec<(&'static str, String)> {
    vec![
        ("Action", "ListBillDetail".into()),
        ("Version", "2022-01-01".into()),
        ("BillPeriod", month.to_string()),
        ("Limit", limit.to_string()),
        ("Offset", offset.to_string()),
        ("GroupTerm", "0".into()),
        ("GroupPeriod", "2".into()),
        ("NeedRecordNum", "1".into()),
        ("IgnoreZero", if ignore_zero { "1" } else { "0" }.into()),
    ]
}

/// A 403 `AccessDenied` means the IAM user lacks the billing read policy.
fn map_error(e: CallError, secret: &Secret) -> ProviderError {
    if let CallError::Http(HttpError::Status { status: 403, body }) = &e
        && serde_json::from_str::<Envelope>(body.as_str()).is_ok_and(|b| {
            b.response_metadata
                .and_then(|m| m.error)
                .is_some_and(|e| e.code == "AccessDenied")
        })
    {
        return super::requirement(format!(
            "BytePlus spend needs an IAM user with the BillingCenterReadOnlyAccess policy; this \
             access key may not read bills. Attach it in the BytePlus console under IAM. \
             BytePlus replied HTTP 403: {body}"
        ));
    }
    e.into_provider(secret)
}

fn page(
    http: &dyn Transport,
    secret: &Secret,
    xdate: &str,
    month: Month,
    limit: u32,
    offset: u64,
    ignore_zero: bool,
) -> Result<Bill, ProviderError> {
    let req = signed_request(secret, xdate, &params(month, limit, offset, ignore_zero))?;
    let env: Envelope = call_json(http, &req, secret).map_err(|e| map_error(e, secret))?;
    if let Some(err) = env.response_metadata.and_then(|m| m.error) {
        return Err(fail(format!(
            "BytePlus returned an error{}",
            super::short_code(&err.code, &[secret])
        )));
    }
    env.result
        .ok_or_else(|| fail("BytePlus returned no Result"))
}

fn bills(
    http: &dyn Transport,
    secret: &Secret,
    xdate: &str,
    month: Month,
    mut each: impl FnMut(Bill) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    // Each page carries the offset it was asked for, since the echoed `Offset` is not trusted.
    // Offsets only grow, so a page that repeats an earlier one is caught by its rows instead.
    let mut seen = std::collections::HashSet::new();
    paged(
        |offset| {
            let offset = offset
                .map_or(Ok(0), str::parse)
                .map_err(|_| fail("bad offset"))?;
            page(http, secret, xdate, month, LIMIT, offset, true).map(|b| (offset, b))
        },
        |(offset, b)| {
            let next = offset + b.list.len() as u64;
            match (next < b.total, b.list.is_empty()) {
                _ if b.total == 0 && !b.list.is_empty() => {
                    Err(fail("BytePlus returned bill rows without a Total"))
                }
                (false, _) => Ok(None),
                (true, true) => Err(fail("BytePlus reported more rows but returned none")),
                (true, false) => Ok(Some(next.to_string())),
            }
        },
        |(_, b)| {
            if !seen.insert(fingerprint(&b.list)) {
                return Err(fail("BytePlus returned the same bill page twice"));
            }
            each(b)
        },
    )
}

impl BytePlus {
    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
    ) -> Result<MonthCache, ProviderError> {
        let now = (self.now)();
        super::window(month, day_of(&now)?)?;
        let xdate = x_date(&now)?;
        let mut cache = MonthCache::new("byteplus", month, FigureKind::Billed, Breakdown::Model);
        bills(http, &credential.secret, &xdate, month, |bill| {
            for row in bill.list {
                if !SEEDANCE.contains(&row.product.as_str()) {
                    continue;
                }
                super::usd(row.currency.as_deref().unwrap_or(""))?;
                // The charge after discounts and coupons, paid or still owed; live rows carry no PayableAmount.
                let (Some(paid), Some(unpaid)) = (row.paid_amount, row.unpaid_amount) else {
                    return Err(fail("a BytePlus bill row lacks PaidAmount or UnpaidAmount"));
                };
                let amount = paid.0 + unpaid.0;
                let day = day_of(&row.expense_date)?;
                if day.month() != month {
                    return Err(fail(format!(
                        "a BytePlus bill row for {month} is dated in another month"
                    )));
                }
                let model = if row.config_name.is_empty() {
                    row.product.clone()
                } else {
                    row.config_name.clone()
                };
                // The model alone would merge two accounts' rows into one, so the owner scopes it.
                let owner = match &row.owner_id {
                    Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    _ => credential.label.clone(),
                };
                let id = format!("{model} ({owner})");
                cache.names.entry(id.clone()).or_insert(model);
                cache.add(day, &id, amount);
            }
            Ok(())
        })?;
        Ok(cache)
    }
}

impl Provider for BytePlus {
    fn platform(&self) -> &'static str {
        "byteplus"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, _config: &Config) -> CheckOutcome {
        let now = (self.now)();
        check_outcome(
            day_of(&now)
                .and_then(|today| x_date(&now).map(|x| (today.month(), x)))
                .and_then(|(month, xdate)| page(http, credential, &xdate, month, 1, 0, false))
                .map(|b| format!("read this month's bill ({} rows)", b.total)),
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

    const AK: &str = "fk-ak-TESTACCESSKEY0001";
    const SK: &str = "fk-secret-TESTSECRETKEY000000000000000000";
    const PAGE1: &str = include_str!("../../fixtures/byteplus/bill_page1.json");
    const PAGE2: &str = include_str!("../../fixtures/byteplus/bill_page2.json");
    const MISSING: &str = include_str!("../../fixtures/byteplus/error_missing_parameter.json");
    const ERR_403: &str = include_str!("../../fixtures/byteplus/error_403.json");
    const ERR_401: &str = include_str!("../../fixtures/byteplus/error_401.json");

    fn key() -> String {
        format!("{AK}:{SK}")
    }

    fn provider() -> BytePlus {
        BytePlus {
            now: || "2026-11-05T12:34:56Z".into(),
        }
    }

    fn fetch(mock: &MockSeq, month: &str) -> Result<MonthCache, ProviderError> {
        provider().fetch_month(
            &guarded(mock),
            &[cred("seedance", &key())],
            Month::parse(month).unwrap(),
            &Config::default(),
        )
    }

    /// The query `fetch` sends for October's first page, already in canonical order.
    const QUERY: &str = "Action=ListBillDetail&BillPeriod=2026-10&GroupPeriod=2&GroupTerm=0&IgnoreZero=1&Limit=300&NeedRecordNum=1&Offset=0&Version=2022-01-01";

    #[test]
    fn signer_matches_a_fixed_vector() {
        assert_eq!(
            canonical_query(&params(Month::parse("2026-10").unwrap(), 300, 0, true)),
            QUERY
        );
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            canonical_request(QUERY, "20261105T123456Z", empty),
            format!(
                "GET\n/\n{QUERY}\ncontent-type:application/x-www-form-urlencoded; charset=utf-8\n\
                 host:open.byteplusapi.com\nx-content-sha256:{empty}\nx-date:20261105T123456Z\n\n\
                 content-type;host;x-content-sha256;x-date\n{empty}"
            )
        );
        // Computed outside this crate with Python's hmac and hashlib, so a change to canonicalization,
        // scope or key derivation fails here.
        let (scope, sig) = signature(SK, "20261105T123456Z", QUERY);
        assert_eq!(scope, "20261105/ap-singapore-1/billing/request");
        assert_eq!(
            sig,
            "7e27c7cb0659fed8ca2849eb6b8739fe800b9f750578ec7e00c3d311810c01fa"
        );
        // The signing key is an HMAC chain over the raw secret: date, region, service, `request`.
        assert_eq!(
            hex(&hmac(
                &hmac(&hmac(&hmac(SK.as_bytes(), "20261105"), REGION), SERVICE),
                "request"
            )),
            "ba2807fb69702c850299a86e5231ab2b79659e9cb8b8481c4787553d70bf1b39"
        );
    }

    #[test]
    fn pages_by_offset_and_sums_seedance_paid_and_unpaid_per_model() {
        let mock = MockSeq::new(vec![
            ("Offset=0", 200, PAGE1.into()),
            ("Offset=2", 200, PAGE2.into()),
        ]);
        let c = fetch(&mock, "2026-10").unwrap();
        let sent = mock.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0].url,
            format!("https://open.byteplusapi.com/?{QUERY}")
        );
        assert!(sent[1].url.contains("&Offset=2&"), "{}", sent[1].url);
        assert_eq!(sent[0].header("x-date"), Some("20261105T123456Z"));
        assert_eq!(
            sent[0].header("content-type"),
            Some("application/x-www-form-urlencoded; charset=utf-8")
        );
        let (scope, sig) = signature(SK, "20261105T123456Z", QUERY);
        assert_eq!(
            sent[0].header("Authorization"),
            Some(
                format!(
                    "HMAC-SHA256 Credential={AK}/{scope}, SignedHeaders=content-type;host;x-content-sha256;x-date, Signature={sig}"
                )
                .as_str()
            )
        );
        assert_eq!(c.kind, FigureKind::Billed);
        assert_eq!(c.breakdown, Breakdown::Model);
        let model = "Dreamina-Seedance-2.0 (1000000001)";
        assert_eq!(
            c.daily[&d("2026-10-03")][model],
            dec("1.5"),
            "PaidAmount plus UnpaidAmount, not PretaxAmount"
        );
        assert_eq!(c.names[model], "Dreamina-Seedance-2.0");
        assert_eq!(
            c.daily[&d("2026-10-09")]["Seedance token package (1000000001)"],
            dec("10")
        );
        assert_eq!(total(&c), dec("11.5"), "TOS rows are not Seedance");
    }

    #[test]
    fn next_offset_counts_rows_received_not_the_echoed_offset() {
        // An echoed Offset of 1 would make the next request 3 and skip a row.
        let mock = MockSeq::new(vec![
            (
                "Offset=0",
                200,
                PAGE1.replace("\"Offset\": 0", "\"Offset\": 1"),
            ),
            (
                "Offset=2",
                200,
                PAGE2.replace("\"Offset\": 2", "\"Offset\": 0"),
            ),
        ]);
        let c = fetch(&mock, "2026-10").unwrap();
        assert_eq!(mock.sent().len(), 2);
        assert!(mock.unused().is_empty(), "{:?}", mock.unused());
        assert_eq!(total(&c), dec("11.5"));
    }

    #[test]
    fn a_page_repeating_any_earlier_page_is_refused() {
        // Page 3, at a new offset, echoes page 1's rows; summing them would count page 1 twice.
        let six = |p: &str| p.replace("\"Total\": 4", "\"Total\": 6");
        let mock = MockSeq::new(vec![
            ("Offset=0", 200, six(PAGE1)),
            ("Offset=2", 200, six(PAGE2)),
            ("Offset=4", 200, six(PAGE1)),
        ]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert!(
            e.message.as_str().contains("same bill page twice"),
            "{}",
            e.message
        );
        assert!(mock.unused().is_empty(), "{:?}", mock.unused());
    }

    #[test]
    fn equal_charges_on_other_resources_are_not_a_repeat() {
        // Page 3 bills the same amounts as page 1, but for other instances.
        let six = |p: &str| p.replace("\"Total\": 4", "\"Total\": 6");
        let other = six(PAGE1)
            .replace("fk-instance-0001", "fk-instance-0002")
            .replace("fk-bucket-0001", "fk-bucket-0002");
        let mock = MockSeq::new(vec![
            ("Offset=0", 200, six(PAGE1)),
            ("Offset=2", 200, six(PAGE2)),
            ("Offset=4", 200, other),
        ]);
        assert!(fetch(&mock, "2026-10").is_ok());
    }

    #[test]
    fn a_row_without_paid_or_unpaid_amount_is_an_error() {
        for drop in ["\"PaidAmount\": \"1.00\",", "\"UnpaidAmount\": \"0.25\","] {
            let page = PAGE1
                .replace("\"Total\": 4", "\"Total\": 2")
                .replacen(drop, "", 1);
            assert!(!page.contains(drop), "{drop}");
            let mock = MockSeq::new(vec![("Offset=0", 200, page)]);
            let e = fetch(&mock, "2026-10").unwrap_err();
            assert!(
                e.message
                    .as_str()
                    .contains("lacks PaidAmount or UnpaidAmount"),
                "{}",
                e.message
            );
        }
    }

    #[test]
    fn rows_without_a_total_are_an_error() {
        for body in [
            PAGE1.replace("\"Total\": 4", "\"Total\": 0"),
            PAGE1.replace("\"Total\": 4,", ""),
        ] {
            assert!(!body.contains("\"Total\": 4"), "{body}");
            let mock = MockSeq::new(vec![("Offset=0", 200, body)]);
            let e = fetch(&mock, "2026-10").unwrap_err();
            assert!(
                e.message.as_str().contains("without a Total"),
                "{}",
                e.message
            );
        }
    }

    #[test]
    fn non_usd_and_missing_parameter_are_errors() {
        let eur = PAGE2
            .replace("\"USD\"", "\"EUR\"")
            .replace("\"Offset\": 2", "\"Offset\": 0")
            .replace("\"Total\": 4", "\"Total\": 2");
        let mock = MockSeq::new(vec![("Offset=0", 200, eur)]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert!(e.message.as_str().contains("USD only"), "{}", e.message);
        let mock = MockSeq::new(vec![("Offset=0", 400, MISSING.into())]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert_eq!(e.requirement, None);
        assert!(e.message.as_str().contains("HTTP 400"), "{}", e.message);
        assert!(
            e.message.as_str().contains("MissingParameter"),
            "{}",
            e.message
        );
    }

    #[test]
    fn access_denied_is_a_requirement_row() {
        let mock = MockSeq::new(vec![("Offset=0", 403, ERR_403.replace("{AK}", AK))]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert_no_key(&e, AK);
        assert!(
            e.requirement
                .unwrap()
                .contains("BillingCenterReadOnlyAccess")
        );
        let check = provider().check(
            &guarded(&MockSeq::new(vec![("Offset=0", 403, ERR_403.into())])),
            &Secret::new(key()),
            &Config::default(),
        );
        assert!(matches!(check, CheckOutcome::Requirement(_)), "{check:?}");
    }

    #[test]
    fn unauthorized_body_with_both_halves_is_scrubbed() {
        let mock = MockSeq::new(vec![(
            "Offset=0",
            401,
            ERR_401.replace("{AK}", AK).replace("{SK}", SK),
        )]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert_no_key(&e, AK);
        assert_no_key(&e, SK);
        assert!(
            e.message.as_str().contains("SignatureDoesNotMatch"),
            "{}",
            e.message
        );
    }

    #[test]
    fn malformed_credential_and_redirect() {
        let mock = MockSeq::new(vec![]);
        let e = provider()
            .fetch_month(
                &guarded(&mock),
                &[cred("x", "fk-only-one-half")],
                Month::parse("2026-10").unwrap(),
                &Config::default(),
            )
            .unwrap_err();
        assert!(e.message.as_str().contains("pair"), "{}", e.message);
        assert!(mock.sent().is_empty());
        let mock = MockSeq::new(vec![("Offset=0", 301, String::new())]);
        let e = fetch(&mock, "2026-10").unwrap_err();
        assert!(
            e.message.as_str().contains("redirect refused"),
            "{}",
            e.message
        );
    }

    #[test]
    fn check_reads_one_row_of_this_month() {
        let ok = MockSeq::new(vec![(
            "Offset=0",
            200,
            PAGE1.replace("\"Total\": 4", "\"Total\": 2"),
        )]);
        let out = provider().check(&guarded(&ok), &Secret::new(key()), &Config::default());
        assert_eq!(
            out,
            CheckOutcome::Ok("read this month's bill (2 rows)".into())
        );
        let url = &ok.sent()[0].url;
        assert!(url.contains("BillPeriod=2026-11&"), "{url}");
        assert!(url.contains("&IgnoreZero=0&Limit=1&"), "{url}");
    }
}
