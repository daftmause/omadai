//! Gemini (Google): spend net of credits per Google Cloud project, read from the Cloud Billing
//! standard usage cost export in BigQuery with the signed-in user's own OAuth client.

use super::{
    CallError, MAX_PAGES, UNATTRIBUTED, bearer, before_history, call_json, check_outcome, enc,
    fail, fetch_each, requirement, usd, window,
};
use crate::cache::{Breakdown, FigureKind, MonthCache};
use crate::config::{Config, GeminiConfig};
use crate::date::{Date, Month};
use crate::google;
use crate::http::{HttpError, Request, Transport};
use crate::provider::{CheckOutcome, Credential, Provider, ProviderError};
use crate::secret::Secret;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::time::{Duration, Instant};

const BASE: &str = "https://bigquery.googleapis.com/bigquery/v2/projects";
/// The bound on one fetch's waiting and paging together.
pub const DEADLINE: Duration = Duration::from_secs(30);
/// BigQuery's own wait per call, inside `http::TIMEOUT`.
const WAIT_MS: u32 = 8000;

pub struct Gemini {
    pub today: fn() -> Date,
    pub now: fn() -> Instant,
}

impl Default for Gemini {
    fn default() -> Self {
        Gemini {
            today: Date::today_utc,
            now: Instant::now,
        }
    }
}

/// A Google Cloud project ID, including a domain-scoped one such as `example.com:project`.
pub fn valid_project(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b':')
        })
}

/// A BigQuery dataset or table name as the export writes them.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A BigQuery location such as `US` or `europe-west1`.
pub fn valid_location(l: &str) -> bool {
    !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn export(config: &Config) -> Result<&GeminiConfig, ProviderError> {
    let Some(g) = &config.gemini else {
        return Err(requirement(
            "Gemini needs its billing export in config.toml: run omadai key add gemini".into(),
        ));
    };
    for (field, ok) in [
        ("project", valid_project(&g.project)),
        ("dataset", valid_name(&g.dataset)),
        ("table", valid_name(&g.table)),
        ("location", valid_location(&g.location)),
    ] {
        if !ok {
            return Err(fail(format!(
                "the Gemini {field} in config.toml is not a valid BigQuery {field}"
            )));
        }
    }
    Ok(g)
}

fn table_path(g: &GeminiConfig) -> String {
    format!("`{}.{}.{}`", g.project, g.dataset, g.table)
}

fn month_sql(g: &GeminiConfig, limit: bool) -> String {
    // One job returns the month's rows and the export's first usage day; `f` always holds one row,
    // so an empty month still comes back as one row carrying only `first`.
    let mut sql = format!(
        "WITH f AS (SELECT FORMAT_DATE('%F', MIN(DATE(usage_start_time))) AS first FROM {t} WHERE cost_type = 'regular'),
m AS (SELECT project.id AS id, ANY_VALUE(project.name) AS name, FORMAT_DATE('%F', DATE(usage_start_time)) AS day,
  CAST(SUM(CAST(cost AS NUMERIC)) + SUM(IFNULL((SELECT SUM(CAST(c.amount AS NUMERIC)) FROM UNNEST(credits) c), 0)) AS STRING) AS net,
  FORMAT_TIMESTAMP('%Y-%m-%dT%H:%M:%SZ', MAX(export_time)) AS exported, currency
FROM {t}
WHERE service.description IN ('Gemini API', 'Vertex AI')
  AND DATE(usage_start_time) >= @from AND DATE(usage_start_time) < @to
GROUP BY id, day, currency)
SELECT m.id, m.name, m.day, m.net, m.exported, m.currency, f.first FROM f LEFT JOIN m ON TRUE",
        t = table_path(g)
    );
    if limit {
        sql.push_str("\nLIMIT 1");
    }
    sql
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryResponse {
    #[serde(default)]
    job_complete: bool,
    #[serde(default)]
    job_reference: Option<JobRef>,
    #[serde(default)]
    rows: Vec<Row>,
    #[serde(default)]
    page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobRef {
    job_id: String,
}

#[derive(Deserialize)]
struct Row {
    f: Vec<Cell>,
}

#[derive(Deserialize)]
struct Cell {
    v: Option<String>,
}

impl Row {
    fn cell(&self, i: usize) -> Option<&str> {
        self.f.get(i).and_then(|c| c.v.as_deref())
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    errors: Vec<ErrorItem>,
    /// Google's newer error shape carries reasons such as `ACCESS_TOKEN_SCOPE_INSUFFICIENT` here.
    #[serde(default)]
    details: Vec<ErrorItem>,
}

#[derive(Deserialize)]
struct ErrorItem {
    #[serde(default)]
    reason: String,
}

/// Maps a BigQuery failure: a 403 by its JSON `reason`, a 404 to `not_found`, the rest as is.
pub(crate) fn bigquery_error(
    e: CallError,
    secret: &Secret,
    project: &str,
    dataset: &str,
    not_found: String,
) -> ProviderError {
    if let CallError::Http(HttpError::Status { status, body }) = &e {
        // A body cut to `MAX_ERROR_TEXT` no longer parses, so its reasons are then scanned for.
        let reasons: Vec<String> = serde_json::from_str::<ErrorBody>(body.as_str())
            .map(|b| {
                b.error
                    .errors
                    .into_iter()
                    .chain(b.error.details)
                    .map(|i| i.reason)
                    .collect()
            })
            .unwrap_or_else(|_| super::json_strings(body.as_str(), "reason"));
        let has = |r: &str| reasons.iter().any(|x| x == r);
        match status {
            403 if has("insufficientPermissions") || has("ACCESS_TOKEN_SCOPE_INSUFFICIENT") => {
                return requirement(
                    "the Google sign-in lacks the scope omadai needs; remove the Gemini key and add it again"
                        .into(),
                );
            }
            403 if has("accessNotConfigured") => {
                return requirement(format!(
                    "Gemini needs the BigQuery API enabled on project {project} \
                     (console.cloud.google.com/apis/library/bigquery.googleapis.com?project={project})"
                ));
            }
            403 if has("accessDenied") => {
                return requirement(format!(
                    "Gemini needs the signed-in Google account to hold BigQuery Job User on project \
                     {project} and BigQuery Data Viewer on dataset {dataset}"
                ));
            }
            404 => return requirement(not_found),
            _ => {}
        }
    }
    e.into_provider(secret)
}

/// One fetch's BigQuery calls, under one deadline and one call budget.
struct Job<'a> {
    http: &'a dyn Transport,
    access: &'a Secret,
    /// The stored credential joined with the access token, whose bare form the header never scrubs.
    secret: Secret,
    export: &'a GeminiConfig,
    now: fn() -> Instant,
    deadline: Instant,
    calls: usize,
}

impl Job<'_> {
    fn call(&mut self, req: Request) -> Result<QueryResponse, ProviderError> {
        self.calls += 1;
        if self.calls > MAX_PAGES {
            return Err(fail(format!(
                "BigQuery took more than {MAX_PAGES} calls for one query; stopped"
            )));
        }
        if (self.now)() >= self.deadline {
            return Err(fail(format!(
                "BigQuery did not finish within {} s; the next fetch tries again",
                DEADLINE.as_secs()
            )));
        }
        let req = req.secret_header("Authorization", bearer(self.access));
        let g = self.export;
        call_json(self.http, &req, &self.secret).map_err(|e| {
            bigquery_error(
                e,
                &self.secret,
                &g.project,
                &g.dataset,
                format!(
                    "Gemini cannot find the billing export table {}.{} in project {} at location {}: \
                     check project, dataset, table and location under [gemini] in config.toml, or remove \
                     the Gemini key and add it again",
                    g.dataset, g.table, g.project, g.location
                ),
            )
        })
    }

    fn results(
        &mut self,
        job_id: &str,
        page: Option<&str>,
    ) -> Result<QueryResponse, ProviderError> {
        let g = self.export;
        let mut url = format!(
            "{BASE}/{}/queries/{}?location={}&timeoutMs={WAIT_MS}",
            enc(&g.project),
            enc(job_id),
            enc(&g.location)
        );
        if let Some(p) = page {
            url.push_str(&format!("&pageToken={}", enc(p)));
        }
        self.call(Request::get(url))
    }

    /// Runs `sql`, waits for it, and reads every page; no row is read from an incomplete response.
    fn query(&mut self, sql: &str, range: Option<(Date, Date)>) -> Result<Vec<Row>, ProviderError> {
        let g = self.export;
        let mut body = serde_json::json!({
            "query": sql,
            "useLegacySql": false,
            "location": g.location,
            "timeoutMs": WAIT_MS,
        });
        if let Some((from, to)) = range {
            let date = |name: &str, d: Date| {
                serde_json::json!({
                    "name": name,
                    "parameterType": {"type": "DATE"},
                    "parameterValue": {"value": d.to_string()}
                })
            };
            body["parameterMode"] = "NAMED".into();
            body["queryParameters"] = serde_json::json!([date("from", from), date("to", to)]);
        }
        let req = Request::post(
            format!("{BASE}/{}/queries", enc(&g.project)),
            Secret::new(body.to_string()),
        )
        .header("content-type", "application/json");
        let mut resp = self.call(req)?;
        let job_id = resp
            .job_reference
            .as_ref()
            .map(|j| j.job_id.clone())
            .filter(|j| !j.is_empty())
            .ok_or_else(|| fail("BigQuery returned no job reference"))?;
        while !resp.job_complete {
            resp = self.results(&job_id, None)?;
        }
        let mut rows = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        loop {
            rows.append(&mut resp.rows);
            match resp.page_token.take().filter(|t| !t.is_empty()) {
                None => return Ok(rows),
                Some(t) if seen.contains(&t) => {
                    return Err(fail("BigQuery repeated a page token"));
                }
                Some(t) => {
                    resp = self.results(&job_id, Some(&t))?;
                    seen.push(t);
                    if !resp.job_complete {
                        return Err(fail("BigQuery returned an incomplete page"));
                    }
                }
            }
        }
    }
}

const EMPTY_EXPORT: &str = "the Gemini billing export holds no data yet; Google fills it within a day \
                            of turning on the standard usage cost export";

impl Gemini {
    fn job<'a>(
        &self,
        http: &'a dyn Transport,
        access: &'a Secret,
        secret: &'a Secret,
        export: &'a GeminiConfig,
    ) -> Job<'a> {
        Job {
            http,
            access,
            secret: Secret::join(&[secret, access]),
            export,
            now: self.now,
            deadline: (self.now)() + DEADLINE,
            calls: 0,
        }
    }

    fn one(
        &self,
        http: &dyn Transport,
        credential: &Credential,
        month: Month,
        config: &Config,
    ) -> Result<MonthCache, ProviderError> {
        let g = export(config)?;
        let range = window(month, (self.today)())?;
        let secret = &credential.secret;
        let access = google::access_token(http, secret)?;
        let mut job = self.job(http, &access, secret, g);
        let rows = job.query(&month_sql(g, false), Some(range))?;
        let first = match rows.first().and_then(|r| r.cell(6)) {
            None => return Err(requirement(EMPTY_EXPORT.into())),
            Some(day) => {
                Date::parse(day).ok_or_else(|| fail("the export's first day is not a date"))?
            }
        };
        // The row the join adds for an empty month carries no day.
        let rows: Vec<&Row> = rows.iter().filter(|r| r.cell(2).is_some()).collect();
        if rows.is_empty() && first > month.last_day() {
            return Err(before_history(first));
        }
        let mut cache = MonthCache::new("gemini", month, FigureKind::Billed, Breakdown::Project);
        if first > month.first_day() && first <= month.last_day() {
            cache.history_from = Some(first);
        }
        let mut latest: Option<String> = None;
        for row in rows {
            let currency = row
                .cell(5)
                .ok_or_else(|| fail("BigQuery returned a row without a currency"))?;
            usd(currency)?;
            let id = row
                .cell(0)
                .filter(|s| !s.is_empty())
                .unwrap_or(UNATTRIBUTED);
            let day = row
                .cell(2)
                .and_then(Date::parse)
                .ok_or_else(|| fail("BigQuery returned a row without a day"))?;
            let net = row
                .cell(3)
                .and_then(|n| Decimal::from_str_exact(n).ok())
                .ok_or_else(|| fail("BigQuery returned a row without a decimal amount"))?;
            cache.add(day, id, net);
            if let Some(name) = row.cell(1).filter(|n| !n.is_empty())
                && id != UNATTRIBUTED
            {
                cache.names.insert(id.to_string(), name.to_string());
            }
            if let Some(at) = row.cell(4)
                && latest.as_deref().is_none_or(|l| at > l)
            {
                latest = Some(at.to_string());
            }
        }
        // The fetch boundary never sees the access token, so ids and names are checked against it here.
        let check = crate::scrub::KeyCheck::new(&[&access]);
        if crate::fetch::key_in_ids(&cache, None, &check).is_some() {
            return Err(fail(
                "Google returned key material in an id or name; nothing was saved",
            ));
        }
        cache.data_as_of =
            latest.map(|at| crate::scrub::scrub(&at, &[&access]).as_str().to_string());
        Ok(cache)
    }
}

impl Provider for Gemini {
    fn platform(&self) -> &'static str {
        "gemini"
    }

    fn check(&self, http: &dyn Transport, credential: &Secret, config: &Config) -> CheckOutcome {
        let today = (self.today)();
        check_outcome((|| {
            let g = export(config)?;
            let range = window(today.month(), today)?;
            let access = google::access_token(http, credential)?;
            let rows = self
                .job(http, &access, credential, g)
                .query(&month_sql(g, true), Some(range))?;
            if rows.first().and_then(|r| r.cell(6)).is_none() {
                return Err(requirement(EMPTY_EXPORT.into()));
            }
            Ok("read this month's Gemini spend".to_string())
        })())
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
    use super::*;
    use crate::http::mock::MockSeq;
    use crate::providers::testutil::{assert_no_key, cred, d, dec, guarded, total};

    const KEY: &str = "fk-gclient.apps.example:fk-gsecret-ABCDEFGH:fk-refresh-0123456789ABCDEF";
    const ACCESS: &str = include_str!("../../fixtures/gemini/token_refresh.json");
    const ONE_PAGE: &str = include_str!("../../fixtures/gemini/query_one_page.json");
    const PAGE1: &str = include_str!("../../fixtures/gemini/query_page1.json");
    const PAGE2: &str = include_str!("../../fixtures/gemini/query_page2.json");
    const PENDING: &str = include_str!("../../fixtures/gemini/query_pending.json");
    const EMPTY: &str = include_str!("../../fixtures/gemini/query_empty.json");
    const DENIED: &str = include_str!("../../fixtures/gemini/error_403_access_denied.json");
    const DISABLED: &str = include_str!("../../fixtures/gemini/error_403_not_configured.json");
    const NOT_FOUND: &str = include_str!("../../fixtures/gemini/error_404.json");

    fn config() -> Config {
        Config {
            gemini: Some(GeminiConfig {
                project: "example-studio-1234".into(),
                dataset: "billing_export".into(),
                table: "gcp_billing_export_v1_000000_AAAAAA_BBBBBB".into(),
                location: "US".into(),
            }),
            ..Config::default()
        }
    }

    fn provider() -> Gemini {
        Gemini {
            today: || Date::parse("2026-11-07").unwrap(),
            now: Instant::now,
        }
    }

    fn fetch(
        p: &Gemini,
        replies: Vec<(&str, u16, String)>,
        month: &str,
        config: &Config,
    ) -> (Result<MonthCache, ProviderError>, MockSeq) {
        let mock = MockSeq::new(replies);
        let r = p.fetch_month(
            &guarded(&mock),
            &[cred("work", KEY)],
            Month::parse(month).unwrap(),
            config,
        );
        (r, mock)
    }

    fn token() -> (&'static str, u16, String) {
        ("oauth2.googleapis.com/token", 200, ACCESS.into())
    }

    #[test]
    fn one_page_is_read_by_project_with_null_names_left_out() {
        let (r, mock) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, ONE_PAGE.into())],
            "2026-10",
            &config(),
        );
        let m = r.unwrap();
        assert!(mock.unused().is_empty(), "{:?}", mock.unused());
        assert_eq!(m.kind, FigureKind::Billed);
        assert_eq!(m.breakdown, Breakdown::Project);
        // NUMERIC strings sum exactly: 0.1 + 0.2 and the long-tailed credit-netted figures.
        let studio: Decimal = m
            .daily
            .values()
            .filter_map(|ids| ids.get("example-studio-1234"))
            .sum();
        assert_eq!(studio, dec("0.3"));
        assert_eq!(total(&m), dec("12.646913578"));
        assert_eq!(m.names["example-studio-1234"], "Example Studio");
        assert!(!m.names.contains_key("example-lab-5678"), "{:?}", m.names);
        assert_eq!(m.data_as_of.as_deref(), Some("2026-11-06T09:30:00Z"));

        let sent = mock.sent();
        assert_eq!(
            sent[1].url,
            "https://bigquery.googleapis.com/bigquery/v2/projects/example-studio-1234/queries"
        );
        assert_eq!(
            sent[1].header("Authorization"),
            Some("Bearer fk-access-REFRESHED0123456789")
        );
        let body: serde_json::Value =
            serde_json::from_str(sent[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["useLegacySql"], false);
        assert_eq!(body["location"], "US");
        assert_eq!(body["parameterMode"], "NAMED");
        assert_eq!(body["timeoutMs"], 8000);
        assert_eq!(
            body["queryParameters"][0]["parameterValue"]["value"],
            "2026-10-01"
        );
        assert_eq!(
            body["queryParameters"][1]["parameterValue"]["value"],
            "2026-11-01"
        );
        let sql = body["query"].as_str().unwrap();
        assert!(
            sql.contains(
                "FROM `example-studio-1234.billing_export.gcp_billing_export_v1_000000_AAAAAA_BBBBBB`"
            ),
            "{sql}"
        );
        // AI Studio and Vertex usage both count; invoice-level tax and rounding rows never do.
        assert!(
            sql.contains("service.description IN ('Gemini API', 'Vertex AI')"),
            "{sql}"
        );
        assert!(!sql.contains("Invoice"), "{sql}");
        assert!(sql.contains("GROUP BY id, day, currency"), "{sql}");
        assert!(!sql.contains("LIMIT"), "{sql}");
    }

    #[test]
    fn two_pages_and_a_pending_job_are_followed() {
        let (r, mock) = fetch(
            &provider(),
            vec![
                token(),
                ("/queries", 200, PENDING.into()),
                (
                    "/queries/job_fk0001?location=US&timeoutMs=8000",
                    200,
                    PAGE1.into(),
                ),
                (
                    "/queries/job_fk0001?location=US&timeoutMs=8000&pageToken=fk-page-2",
                    200,
                    PAGE2.into(),
                ),
            ],
            "2026-10",
            &config(),
        );
        let m = r.unwrap();
        assert!(mock.unused().is_empty(), "{:?}", mock.unused());
        assert_eq!(total(&m), dec("4.000000001"));
        assert_eq!(m.daily.len(), 2);
        let sent = mock.sent();
        assert_eq!(sent[2].method, crate::http::Method::Get);
        assert!(!sent[2].url.contains("pageToken"), "{}", sent[2].url);
    }

    /// An empty month: the one row the join adds, carrying only the export's first day.
    fn empty_from(first: Option<&str>) -> String {
        match first {
            Some(day) => EMPTY.replace("2026-09-01", day),
            None => EMPTY.replace(r#"{"v":"2026-09-01"}"#, r#"{"v":null}"#),
        }
    }

    #[test]
    fn an_empty_month_reads_the_first_export_day() {
        // Data before the month: a real $0.00, from one job.
        let (r, mock) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, empty_from(Some("2026-09-01")))],
            "2026-10",
            &config(),
        );
        let m = r.unwrap();
        assert!(mock.unused().is_empty());
        assert_eq!(mock.sent().len(), 2, "one token grant and one query");
        assert!(m.daily.is_empty(), "{:?}", m.daily);
        assert_eq!(m.history_from, None, "data from before the month");

        // An empty month the export starts inside keeps the first day as its floor.
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, empty_from(Some("2026-10-15")))],
            "2026-10",
            &config(),
        );
        assert_eq!(r.unwrap().history_from, Some(d("2026-10-15")));

        // A month before the first export day is before history.
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, empty_from(Some("2026-09-01")))],
            "2026-08",
            &config(),
        );
        assert_eq!(r.unwrap_err().before_history, Some(d("2026-09-01")));

        // An empty table is a requirement.
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, empty_from(None))],
            "2026-10",
            &config(),
        );
        let req = r.unwrap_err().requirement.unwrap();
        assert!(req.contains("holds no data yet"), "{req}");
    }

    #[test]
    fn the_first_export_day_counts_only_regular_usage() {
        let (_, mock) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, ONE_PAGE.into())],
            "2026-10",
            &config(),
        );
        let body: serde_json::Value =
            serde_json::from_str(mock.sent()[1].body.as_deref().unwrap()).unwrap();
        let sql = body["query"].as_str().unwrap();
        // Invoice-level tax and rounding rows are dated to the first of their month.
        assert!(
            sql.contains("MIN(DATE(usage_start_time))) AS first FROM `example-studio-1234.billing_export.gcp_billing_export_v1_000000_AAAAAA_BBBBBB` WHERE cost_type = 'regular'"),
            "{sql}"
        );
        assert!(sql.contains("FROM f LEFT JOIN m ON TRUE"), "{sql}");
    }

    #[test]
    fn bigquery_refusals_are_requirements() {
        for (status, body, want) in [
            (
                403,
                DENIED,
                "BigQuery Job User on project example-studio-1234",
            ),
            (
                403,
                DISABLED,
                "BigQuery API enabled on project example-studio-1234",
            ),
            (
                404,
                NOT_FOUND,
                "billing_export.gcp_billing_export_v1_000000_AAAAAA_BBBBBB",
            ),
        ] {
            let (r, _) = fetch(
                &provider(),
                vec![token(), ("/queries", status, body.into())],
                "2026-10",
                &config(),
            );
            let e = r.unwrap_err();
            let req = e.requirement.clone().unwrap_or_default();
            assert!(req.contains(want), "{status}: {e:?}");
            assert_no_key(&e, "fk-access-REFRESHED0123456789");
        }
        // A 403 with another reason stays a failure.
        let (r, _) = fetch(
            &provider(),
            vec![
                token(),
                (
                    "/queries",
                    403,
                    r#"{"error":{"errors":[{"reason":"quotaExceeded"}]}}"#.into(),
                ),
            ],
            "2026-10",
            &config(),
        );
        assert_eq!(r.unwrap_err().requirement, None);
    }

    #[test]
    fn bad_config_identifiers_are_refused_before_any_request() {
        for (field, value) in [
            ("project", "evil`; DROP"),
            ("project", "Upper-Case"),
            ("dataset", "billing.export"),
            ("table", "t`x"),
            ("table", ""),
            ("location", "US&x=1"),
        ] {
            let mut c = config();
            let g = c.gemini.as_mut().unwrap();
            match field {
                "project" => g.project = value.into(),
                "dataset" => g.dataset = value.into(),
                "table" => g.table = value.into(),
                _ => g.location = value.into(),
            }
            let (r, mock) = fetch(&provider(), vec![], "2026-10", &c);
            let e = r.unwrap_err();
            assert!(e.message.as_str().contains(field), "{field}: {e:?}");
            assert!(mock.sent().is_empty(), "{field}: a request was sent");
        }
        assert!(valid_project("example.com:my-project-1"));
        let (r, mock) = fetch(&provider(), vec![], "2026-10", &Config::default());
        assert!(r.unwrap_err().requirement.is_some());
        assert!(mock.sent().is_empty());
    }

    fn ticking() -> Instant {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicU32, Ordering};
        static BASE: OnceLock<Instant> = OnceLock::new();
        static TICKS: AtomicU32 = AtomicU32::new(0);
        let base = *BASE.get_or_init(Instant::now);
        base + Duration::from_secs(11) * TICKS.fetch_add(1, Ordering::SeqCst)
    }

    #[test]
    fn one_deadline_bounds_polling_and_paging() {
        // Each read of the clock moves it 11 s: the query and one poll fit in 30 s, the page does not.
        let p = Gemini {
            today: || Date::parse("2026-11-07").unwrap(),
            now: ticking,
        };
        let (r, mock) = fetch(
            &p,
            vec![
                token(),
                ("/queries", 200, PENDING.into()),
                ("/queries/job_fk0001", 200, PAGE1.into()),
                ("/queries/job_fk0001", 200, PAGE2.into()),
            ],
            "2026-10",
            &config(),
        );
        let e = r.unwrap_err();
        assert!(e.message.as_str().contains("within 30 s"), "{e:?}");
        assert_eq!(mock.unused(), ["/queries/job_fk0001"]);
    }

    #[test]
    fn a_job_that_never_completes_stops_at_the_call_budget() {
        let mut replies = vec![token()];
        replies.extend((0..MAX_PAGES).map(|_| ("/queries", 200, PENDING.to_string())));
        let (r, mock) = fetch(&provider(), replies, "2026-10", &config());
        let e = r.unwrap_err();
        assert!(e.message.as_str().contains("calls"), "{e:?}");
        assert!(mock.unused().is_empty());
        assert_eq!(mock.sent().len(), MAX_PAGES + 1);
    }

    #[test]
    fn check_reads_one_row_of_this_month() {
        let mock = MockSeq::new(vec![token(), ("/queries", 200, ONE_PAGE.into())]);
        let out = provider().check(&guarded(&mock), &Secret::new(KEY.into()), &config());
        assert_eq!(
            out,
            CheckOutcome::Ok("read this month's Gemini spend".into())
        );
        let body: serde_json::Value =
            serde_json::from_str(mock.sent()[1].body.as_deref().unwrap()).unwrap();
        assert!(body["query"].as_str().unwrap().ends_with("LIMIT 1"));
        assert_eq!(
            body["queryParameters"][0]["parameterValue"]["value"],
            "2026-11-01"
        );
        let mock = MockSeq::new(vec![token(), ("/queries", 403, DENIED.into())]);
        let out = provider().check(&guarded(&mock), &Secret::new(KEY.into()), &config());
        assert!(matches!(out, CheckOutcome::Requirement(r) if r.contains("BigQuery Job User")));
    }

    #[test]
    fn check_on_an_empty_export_is_the_fetch_requirement() {
        let mock = MockSeq::new(vec![token(), ("/queries", 200, empty_from(None))]);
        let out = provider().check(&guarded(&mock), &Secret::new(KEY.into()), &config());
        assert!(
            matches!(&out, CheckOutcome::Requirement(r) if r.contains("holds no data yet")),
            "{out:?}"
        );
    }

    #[test]
    fn a_non_usd_export_is_refused() {
        let eur = ONE_PAGE.replacen(r#"{"v":"USD"}"#, r#"{"v":"EUR"}"#, 1);
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, eur)],
            "2026-10",
            &config(),
        );
        let e = r.unwrap_err();
        assert!(
            e.message.as_str().contains("omadai reports USD only"),
            "{e:?}"
        );
    }

    #[test]
    fn a_vertex_ai_row_is_counted() {
        let vertex = ONE_PAGE.replacen("example-lab-5678", "example-vertex-9999", 1);
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, vertex)],
            "2026-10",
            &config(),
        );
        let m = r.unwrap();
        let v: Decimal = m
            .daily
            .values()
            .filter_map(|ids| ids.get("example-vertex-9999"))
            .sum();
        assert_eq!(v, dec("12.346913578"));
    }

    #[test]
    fn a_403_reason_survives_a_body_cut_short() {
        // Past `MAX_ERROR_TEXT` the body is truncated and no longer parses as JSON.
        let long = format!(
            r#"{{"error":{{"code":403,"errors":[{{"reason":"accessDenied","message":"{}"}}]}}}}"#,
            "x".repeat(4096)
        );
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 403, long)],
            "2026-10",
            &config(),
        );
        let e = r.unwrap_err();
        let req = e.requirement.clone().unwrap_or_default();
        assert!(req.contains("BigQuery Job User"), "{e:?}");
        assert_eq!(
            super::super::json_strings(
                r#"{"reason": "a_b1", "reason":"x y", "reason":"ok"#,
                "reason"
            ),
            ["a_b1"]
        );
    }

    #[test]
    fn a_bigquery_error_echoing_the_bare_access_token_leaves_no_trace() {
        let (r, _) = fetch(
            &provider(),
            vec![
                token(),
                (
                    "/queries",
                    400,
                    r#"{"error":{"message":"invalid token fk-access-REFRESHED0123456789"}}"#.into(),
                ),
            ],
            "2026-10",
            &config(),
        );
        assert_no_key(&r.unwrap_err(), "fk-access-REFRESHED0123456789");
    }

    #[test]
    fn a_result_naming_a_project_with_the_access_token_saves_nothing() {
        let leaked = ONE_PAGE.replace("Example Studio", "Studio fk-access-REFRESHED0123456789");
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, leaked)],
            "2026-10",
            &config(),
        );
        let e = r.unwrap_err();
        assert!(e.message.as_str().contains("key material"), "{e:?}");
        assert_no_key(&e, "fk-access-REFRESHED0123456789");
    }

    #[test]
    fn a_data_as_of_holding_the_access_token_is_scrubbed() {
        let leaked = ONE_PAGE.replace(
            "2026-11-06T09:30:00Z",
            "2026-11-06T09:30:00Z fk-access-REFRESHED0123456789",
        );
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, leaked)],
            "2026-10",
            &config(),
        );
        let m = r.unwrap();
        let at = m.data_as_of.unwrap();
        assert!(!at.contains("fk-access"), "{at}");
    }

    #[test]
    fn a_month_the_export_starts_inside_keeps_its_first_day() {
        let started = ONE_PAGE.replace(r#"{"v":"2026-09-01"}"#, r#"{"v":"2026-10-15"}"#);
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, started.clone())],
            "2026-10",
            &config(),
        );
        assert_eq!(r.unwrap().history_from, Some(d("2026-10-15")));
        // A first day on the month's first day, or before it, leaves no floor.
        let whole = ONE_PAGE.to_string();
        let (r, mock) = fetch(
            &provider(),
            vec![token(), ("/queries", 200, whole)],
            "2026-10",
            &config(),
        );
        assert_eq!(r.unwrap().history_from, None);
        let body: serde_json::Value =
            serde_json::from_str(mock.sent()[1].body.as_deref().unwrap()).unwrap();
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("MIN(DATE(usage_start_time))")
        );
    }

    #[test]
    fn a_403_for_a_missing_scope_is_a_requirement_in_either_shape() {
        for body in [
            r#"{"error":{"code":403,"errors":[{"reason":"insufficientPermissions"}]}}"#,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#,
        ] {
            let (r, _) = fetch(
                &provider(),
                vec![token(), ("/queries", 403, body.into())],
                "2026-10",
                &config(),
            );
            let e = r.unwrap_err();
            let req = e.requirement.clone().unwrap_or_default();
            assert!(req.contains("lacks the scope"), "{body}: {e:?}");
        }
    }

    #[test]
    fn a_404_names_the_location_too() {
        let (r, _) = fetch(
            &provider(),
            vec![token(), ("/queries", 404, NOT_FOUND.into())],
            "2026-10",
            &config(),
        );
        let req = r.unwrap_err().requirement.unwrap();
        assert!(req.contains("at location US"), "{req}");
    }
}
