//! The built-in providers and what they share: JSON calls, error mapping, date windows and merging.

pub mod anthropic;
pub mod byteplus;
pub mod derived;
pub mod fal;
pub mod fish;
pub mod flux;
pub mod gemini;
pub mod higgsfield;
pub mod openai;
pub mod xai;

use crate::cache::MonthCache;
use crate::date::{Date, Month};
use crate::http::{HttpError, Request, Transport};
use crate::provider::{Cause, CheckOutcome, Credential, Provider, ProviderError};
use crate::scrub::{Scrubbed, plain, scrub};
use crate::secret::Secret;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// The ID spend is filed under when the platform attributes it to no key.
pub const UNATTRIBUTED: &str = "unattributed";

/// The ID prefix for spend a platform attributes to a signed-in user rather than a key (fal.ai).
pub const USER_PREFIX: &str = "user:";

/// The most pages one listing may take before omadai assumes the cursor is looping.
const MAX_PAGES: usize = 500;

pub fn builtin() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(openai::OpenAi::default()),
        Box::new(anthropic::Anthropic::default()),
        Box::new(fal::Fal::default()),
        Box::new(xai::Xai::default()),
        Box::new(higgsfield::Higgsfield::default()),
        Box::new(byteplus::BytePlus::default()),
        Box::new(flux::Flux::default()),
        Box::new(fish::Fish::default()),
        Box::new(gemini::Gemini::default()),
    ]
}

/// A decimal read from a JSON number or string without passing through `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dec(pub Decimal);

impl<'de> Deserialize<'de> for Dec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        rust_decimal::serde::arbitrary_precision::deserialize(d).map(Dec)
    }
}

/// Divides by 100 by moving the decimal point, so no rounding can occur.
pub fn cents_to_dollars(cents: Decimal) -> Decimal {
    let mut d = cents;
    if d.scale() <= 26 {
        d.set_scale(d.scale() + 2).expect("scale checked");
        d.normalize()
    } else {
        (cents / Decimal::ONE_HUNDRED).normalize()
    }
}

pub fn fail(message: impl Into<String>) -> ProviderError {
    ProviderError {
        message: plain(message),
        requirement: None,
        cause: Cause::Other,
        before_history: None,
    }
}

/// A requirement row: the text is ours plus an already scrubbed vendor reason.
pub fn requirement(text: String) -> ProviderError {
    ProviderError {
        message: plain(text.clone()),
        requirement: Some(text),
        cause: Cause::Other,
        before_history: None,
    }
}

/// A configured label with no stored secret, so the log can say to add the key again.
pub fn missing_credential(message: impl Into<String>) -> ProviderError {
    ProviderError {
        cause: Cause::MissingCredential,
        ..fail(message)
    }
}

/// A month before the platform's history, which starts on `first`; `fetch::run` caches it as such.
pub fn before_history(first: Date) -> ProviderError {
    ProviderError {
        message: plain(format!("no history before {first}")),
        requirement: None,
        cause: Cause::Other,
        before_history: Some(first),
    }
}

pub fn from_http(e: &HttpError, secret: &Secret) -> ProviderError {
    let cause = match e {
        HttpError::Status { status, .. } => Cause::Status(*status),
        HttpError::Redirect(status) => Cause::Redirect(*status),
        HttpError::Transport(_) => Cause::Transport,
        HttpError::Refused(_) => Cause::Refused,
    };
    ProviderError {
        message: scrub(&e.to_string(), &[secret]),
        requirement: None,
        cause,
        before_history: None,
    }
}

pub enum CallError {
    Http(HttpError),
    Parse(Scrubbed),
}

impl CallError {
    pub fn into_provider(self, secret: &Secret) -> ProviderError {
        match self {
            CallError::Http(e) => from_http(&e, secret),
            CallError::Parse(m) => ProviderError {
                message: m,
                requirement: None,
                cause: Cause::Other,
                before_history: None,
            },
        }
    }
}

pub fn call_json<T: DeserializeOwned>(
    http: &dyn Transport,
    request: &Request,
    secret: &Secret,
) -> Result<T, CallError> {
    let response = http.send(request, secret).map_err(CallError::Http)?;
    serde_json::from_slice(&response.body).map_err(|e| {
        CallError::Parse(scrub(
            &format!("unexpected response from the platform: {e}"),
            &[secret],
        ))
    })
}

/// The bearer value for an `Authorization` header, in its own zeroizing buffer.
pub fn bearer(secret: &Secret) -> Secret {
    Secret::with_prefix("Bearer ", secret)
}

/// Percent-encodes a query or path value, leaving only RFC 3986 unreserved characters.
pub fn enc(value: &str) -> String {
    let mut out = String::with_capacity(enc_len(value));
    enc_into(&mut out, value);
    out
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// The length of `enc(value)`, so a caller can size a zeroizing buffer before `enc_into`.
pub fn enc_len(value: &str) -> usize {
    value
        .bytes()
        .map(|b| if unreserved(b) { 1 } else { 3 })
        .sum()
}

/// Appends `enc(value)` to `out` with no intermediate copy.
pub fn enc_into(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in value.bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 15)] as char);
        }
    }
}

/// Pages a cursor listing until the platform says there is no more, refusing a cursor that loops.
pub fn paged<P>(
    mut fetch: impl FnMut(Option<&str>) -> Result<P, ProviderError>,
    next: impl Fn(&P) -> Result<Option<String>, ProviderError>,
    mut each: impl FnMut(P) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    let mut cursor: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = fetch(cursor.as_deref())?;
        let following = next(&page)?;
        each(page)?;
        match following {
            None => return Ok(()),
            Some(c) if seen.contains(&c) => {
                return Err(fail("the platform repeated a page cursor"));
            }
            Some(c) => {
                seen.push(c.clone());
                cursor = Some(c);
            }
        }
    }
    Err(fail(format!("more than {MAX_PAGES} pages; stopped")))
}

/// A hash of one page's content, so a listing can refuse a page equal to any earlier one.
pub fn fingerprint(page: &impl std::hash::Hash) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut h = DefaultHasher::new();
    page.hash(&mut h);
    h.finish()
}

/// The cursor for the next page: `has_more` with no cursor is an error rather than a silent stop.
pub fn next_cursor(
    has_more: bool,
    cursor: Option<&String>,
) -> Result<Option<String>, ProviderError> {
    match (has_more, cursor) {
        (false, _) => Ok(None),
        (true, Some(c)) if !c.is_empty() => Ok(Some(c.clone())),
        (true, _) => Err(fail("the platform reported more pages but gave no cursor")),
    }
}

/// The days of `month` that have begun, as `[start, end)`: a running month ends after today.
pub fn window(month: Month, today: Date) -> Result<(Date, Date), ProviderError> {
    let start = month.first_day();
    if start > today {
        return Err(fail(format!("{month} has not started yet")));
    }
    let month_end = month.succ().first_day();
    let end = if today.month() == month {
        today.succ()
    } else {
        month_end
    };
    Ok((start, end))
}

pub fn unix(day: Date) -> i64 {
    day.to_days() * 86_400
}

/// The calendar day an RFC 3339 or ISO 8601 timestamp starts with.
pub fn day_of(timestamp: &str) -> Result<Date, ProviderError> {
    timestamp
        .get(..10)
        .and_then(Date::parse)
        .ok_or_else(|| fail("the platform returned a timestamp that is not a date"))
}

/// A vendor's short code, quoted only when it is a plain word of at most 32 characters that `scrub` with `secrets` leaves unchanged.
pub fn short_code(code: &str, secrets: &[&Secret]) -> String {
    if !code.is_empty()
        && code.len() <= 32
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && scrub(code, secrets).as_str() == code
    {
        format!(" {code:?}")
    } else {
        String::new()
    }
}

/// Requires the platform's figures to be in US dollars, the only currency omadai reports.
pub fn usd(currency: &str) -> Result<(), ProviderError> {
    if currency.eq_ignore_ascii_case("usd") {
        return Ok(());
    }
    // A currency code is three letters, so anything else is not quoted.
    let shown = if currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_alphabetic()) {
        format!(" {currency:?}")
    } else {
        String::new()
    };
    Err(fail(format!(
        "unexpected currency{shown}; omadai reports USD only"
    )))
}

/// Every `"<key>": "<word>"` value in text that may be JSON cut short, where a word is letters,
/// digits and `_`; a value holding anything else is skipped.
pub fn json_strings(text: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        rest = &rest[at + needle.len()..];
        let Some(after) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(value) = after.trim_start().strip_prefix('"') else {
            continue;
        };
        let word: String = value
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !word.is_empty() && value[word.len()..].starts_with('"') {
            out.push(word);
        }
    }
    out
}

/// Runs `one` per credential and merges the months; see `merge` for why an ID is never added twice.
///
/// `fetch::run` passes every configured credential or calls no provider, so the set is never empty.
pub fn fetch_each(
    credentials: &[Credential],
    one: impl FnMut(&Credential) -> Result<MonthCache, ProviderError>,
) -> Result<MonthCache, ProviderError> {
    each_merged(credentials, one, false)
}

/// As `fetch_each`, for months whose `credits_remaining` each hold a distinct balance, which add
/// up; a provider leaves it unset on a month that repeats a balance already counted.
pub fn fetch_each_balance(
    credentials: &[Credential],
    one: impl FnMut(&Credential) -> Result<MonthCache, ProviderError>,
) -> Result<MonthCache, ProviderError> {
    each_merged(credentials, one, true)
}

fn each_merged(
    credentials: &[Credential],
    mut one: impl FnMut(&Credential) -> Result<MonthCache, ProviderError>,
    sum_credits: bool,
) -> Result<MonthCache, ProviderError> {
    let mut merged: Option<MonthCache> = None;
    for c in credentials {
        let month = per_credential(one(c).map_err(|e| labelled(&c.label, e))?, &c.label);
        merged = Some(match merged {
            None => month,
            Some(m) => merge(m, month, sum_credits),
        });
    }
    Ok(merged.expect("at least one credential"))
}

fn labelled(label: &str, e: ProviderError) -> ProviderError {
    ProviderError {
        message: plain(format!("key {label:?}: {}", e.message)),
        requirement: e.requirement.map(|r| format!("key {label:?}: {r}")),
        cause: e.cause,
        before_history: e.before_history,
    }
}

/// Files the IDs that name no key under the credential's label, so `merge` keeps each credential's.
// A provider that knows the organization keys those rows by it first, which leaves them unchanged here.
fn per_credential(mut month: MonthCache, label: &str) -> MonthCache {
    let own = |id: String| {
        if id == UNATTRIBUTED || id.starts_with(USER_PREFIX) {
            format!("{id} ({label})")
        } else {
            id
        }
    };
    month.daily = std::mem::take(&mut month.daily)
        .into_iter()
        .map(|(day, ids)| (day, ids.into_iter().map(|(id, a)| (own(id), a)).collect()))
        .collect();
    month.names = std::mem::take(&mut month.names)
        .into_iter()
        .map(|(id, n)| (own(id), n))
        .collect();
    month
}

/// Two credentials for one organization see the same key IDs, and the same organization-keyed
/// unattributed rows, with the same figures, so an ID already present for a day is kept rather
/// than added to; label-keyed rows never collide, so they add up. Credits remaining are one
/// organization's balance seen twice unless `sum_credits` says each month holds its own.
fn merge(mut into: MonthCache, other: MonthCache, sum_credits: bool) -> MonthCache {
    for (day, ids) in other.daily {
        let slot = into.daily.entry(day).or_default();
        for (id, amount) in ids {
            slot.entry(id).or_insert(amount);
        }
    }
    for (id, name) in other.names {
        into.names.entry(id).or_insert(name);
    }
    into.kind = into.kind.max(other.kind);
    into.credits_remaining = match (into.credits_remaining, other.credits_remaining) {
        (Some(a), Some(b)) if sum_credits => Some(a + b),
        (a, b) => a.or(b),
    };
    into.billed_total = into.billed_total.or(other.billed_total);
    into.warnings.extend(other.warnings);
    into.credits_warnings.extend(other.credits_warnings);
    into.history_from = into.history_from.max(other.history_from);
    into.data_as_of = match (into.data_as_of, other.data_as_of) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    into
}

/// Maps a fetch result onto the one-time `key add` outcome.
pub fn check_outcome(result: Result<String, ProviderError>) -> CheckOutcome {
    match result {
        Ok(what) => CheckOutcome::Ok(what),
        Err(ProviderError {
            requirement: Some(r),
            ..
        }) => CheckOutcome::Requirement(r),
        Err(e) => CheckOutcome::Failed(e.message),
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use crate::cache::{Breakdown, FigureKind};
    use crate::http::Guarded;
    use crate::http::mock::MockSeq;

    pub fn d(s: &str) -> Date {
        Date::parse(s).unwrap()
    }

    pub fn dec(s: &str) -> Decimal {
        Decimal::from_str_exact(s).unwrap()
    }

    /// A credential whose series ID is `series-<label>`.
    pub fn cred(label: &str, key: &str) -> Credential {
        Credential {
            label: label.into(),
            series: format!("series-{label}"),
            secret: Secret::new(key.into()),
        }
    }

    pub fn guarded(mock: &MockSeq) -> Guarded<&MockSeq> {
        Guarded::new(mock)
    }

    pub fn total(c: &MonthCache) -> Decimal {
        c.daily.values().flat_map(|m| m.values()).sum()
    }

    /// Asserts that neither the key nor its 8-character prefix survives in any rendering of an error.
    pub fn assert_no_key(e: &ProviderError, key: &str) {
        let prefix = &key[..8];
        for text in [
            e.message.to_string(),
            format!("{e:?}"),
            serde_json::to_string(&e.message).unwrap(),
            e.requirement.clone().unwrap_or_default(),
        ] {
            assert!(!text.contains(key), "key leaked: {text}");
            assert!(!text.contains(prefix), "key prefix leaked: {text}");
        }
    }

    #[test]
    fn cents_convert_exactly() {
        assert_eq!(cents_to_dollars(dec("41280.000000")), dec("412.8"));
        assert_eq!(cents_to_dollars(dec("1.5")), dec("0.015"));
        assert_eq!(cents_to_dollars(dec("-1000")), dec("-10"));
        let mut sum = Decimal::ZERO;
        for _ in 0..10 {
            sum += cents_to_dollars(dec("0.1"));
        }
        assert_eq!(sum, dec("0.01"));
    }

    #[test]
    fn windows_and_encoding() {
        let nov = Month::parse("2026-11").unwrap();
        assert_eq!(
            window(nov, d("2026-11-05")).unwrap(),
            (d("2026-11-01"), d("2026-11-06"))
        );
        let oct = Month::parse("2026-10").unwrap();
        assert_eq!(
            window(oct, d("2026-11-05")).unwrap(),
            (d("2026-10-01"), d("2026-11-01"))
        );
        assert!(window(Month::parse("2026-12").unwrap(), d("2026-11-05")).is_err());
        assert_eq!(enc("a b/c=+~"), "a%20b%2Fc%3D%2B~");
        assert_eq!(unix(d("1970-01-02")), 86_400);
    }

    #[test]
    fn same_key_from_two_credentials_is_not_double_counted() {
        let nov = Month::parse("2026-11").unwrap();
        let creds = [
            cred("a", "fk-one-000000000000"),
            cred("b", "fk-two-000000000000"),
        ];
        let merged = fetch_each(&creds, |c| {
            let mut m = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
            m.add(d("2026-11-02"), "key_shared", dec("1.25"));
            if c.label == "b" {
                m.add(d("2026-11-02"), "key_only_b", dec("2"));
            }
            Ok(m)
        })
        .unwrap();
        assert_eq!(total(&merged), dec("3.25"));
    }

    /// One OpenAI costs page holding a single unattributed result for `org`.
    fn openai_unattributed(org: &str, value: &str) -> String {
        format!(
            r#"{{"object":"page","has_more":false,"data":[{{"object":"bucket","start_time":1793664000,"end_time":1793750400,"results":[{{"object":"organization.costs.result","api_key_id":null,"amount":{{"value":{value},"currency":"usd"}},"organization_id":"{org}","organization_name":"Example Org"}}]}}]}}"#
        )
    }

    fn openai_two(first: (&str, &str), second: (&str, &str)) -> MonthCache {
        let mock = MockSeq::new(vec![
            ("/costs?", 200, openai_unattributed(first.0, first.1)),
            ("/costs?", 200, openai_unattributed(second.0, second.1)),
        ]);
        let provider = openai::OpenAi {
            today: || Date::parse("2026-11-05").unwrap(),
        };
        let creds = [
            cred("a", "fk-openai-one-000000000000"),
            cred("b", "fk-openai-two-000000000000"),
        ];
        let month = provider
            .fetch_month(
                &guarded(&mock),
                &creds,
                Month::parse("2026-11").unwrap(),
                &crate::config::Config::default(),
            )
            .unwrap();
        assert!(mock.unused().is_empty(), "{:?}", mock.unused());
        month
    }

    #[test]
    fn unattributed_rows_are_grouped_by_organization_where_reported() {
        // Two OpenAI credentials for one organization see its unattributed spend once.
        let same = openai_two(("org-fk-one", "0.5"), ("org-fk-one", "0.5"));
        assert_eq!(total(&same), dec("0.5"), "same organization counted once");
        assert_eq!(
            same.daily[&d("2026-11-03")]["unattributed (org-fk-one)"],
            dec("0.5")
        );
        // Credentials for different organizations add up.
        let different = openai_two(("org-fk-one", "0.5"), ("org-fk-two", "0.75"));
        assert_eq!(
            total(&different),
            dec("1.25"),
            "different organizations summed"
        );
        let day = &different.daily[&d("2026-11-03")];
        assert_eq!(day["unattributed (org-fk-one)"], dec("0.5"));
        assert_eq!(day["unattributed (org-fk-two)"], dec("0.75"));

        // A platform that reports no organization keeps each credential's rows under its label.
        let nov = Month::parse("2026-11").unwrap();
        let creds = [
            cred("a", "fk-one-000000000000"),
            cred("b", "fk-two-000000000000"),
        ];
        let merged = fetch_each(&creds, |_| {
            let mut m = MonthCache::new("fal", nov, FigureKind::Billed, Breakdown::Key);
            m.add(d("2026-11-02"), UNATTRIBUTED, dec("1.00"));
            m.add(d("2026-11-02"), "user:fk-member", dec("0.50"));
            m.names.insert("user:fk-member".into(), "Member".into());
            m.add(d("2026-11-02"), "key_shared", dec("2"));
            Ok(m)
        })
        .unwrap();
        assert_eq!(total(&merged), dec("5.00"));
        let day = &merged.daily[&d("2026-11-02")];
        assert_eq!(day["unattributed (a)"], dec("1.00"));
        assert_eq!(day["unattributed (b)"], dec("1.00"));
        assert_eq!(day["user:fk-member (a)"], dec("0.50"));
        assert_eq!(day["user:fk-member (b)"], dec("0.50"));
        assert_eq!(day["key_shared"], dec("2"));
        assert_eq!(merged.names["user:fk-member (b)"], "Member");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_code_drops_a_code_that_quotes_part_of_the_key() {
        let key = Secret::new("fk-admin-ABCDEFGHIJKLMNOPQRSTUVWXYZ12".into());
        assert_eq!(short_code("fk-admin-ABCDEFGHIJKLMNOPQRSTUV", &[&key]), "");
        assert_eq!(short_code("AccessDenied", &[&key]), " \"AccessDenied\"");
    }

    #[test]
    fn usd_quotes_only_a_three_letter_currency() {
        let Err(e) = usd("fk-admin-ABCDEFGHIJKLMNOPQRSTUV") else {
            panic!("not usd");
        };
        assert!(!e.message.as_str().contains("ABCDEF"), "{}", e.message);
        let Err(e) = usd("EUR") else {
            panic!("not usd")
        };
        assert!(e.message.as_str().contains("\"EUR\""), "{}", e.message);
    }
}
