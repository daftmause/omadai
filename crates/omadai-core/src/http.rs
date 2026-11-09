//! HTTPS to a fixed host allowlist, with no redirects followed and error bodies scrubbed.

use crate::scrub::{Scrubbed, scrub};
use crate::secret::Secret;
use std::fmt;
use std::time::Duration;
use ureq::http::Uri;

/// Every host omadai may contact; matched exactly, over https on the default port only.
pub const ALLOWED_HOSTS: [&str; 10] = [
    "api.openai.com",
    "api.anthropic.com",
    "api.fal.ai",
    "api.higgsfield.ai",
    "bigquery.googleapis.com",
    "oauth2.googleapis.com",
    "open.byteplusapi.com",
    "api.bfl.ai",
    "api.fish.audio",
    "management-api.x.ai",
];

pub const TIMEOUT: Duration = Duration::from_secs(15);
/// The largest response body read; provider responses are far smaller.
pub const MAX_BODY: u64 = 8 * 1024 * 1024;
/// The most error text kept from a response or connection failure, cut after scrubbing.
pub const MAX_ERROR_TEXT: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

pub enum HeaderValue {
    Public(String),
    Secret(Secret),
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderValue::Public(v) => fmt::Debug::fmt(v, f),
            HeaderValue::Secret(_) => f.write_str("[redacted]"),
        }
    }
}

impl HeaderValue {
    fn expose(&self) -> &str {
        match self {
            HeaderValue::Public(v) => v,
            HeaderValue::Secret(s) => s.expose(),
        }
    }
}

/// A request body; may carry a token (an OAuth refresh), so it is zeroed on drop and never printed.
pub struct Body(Secret);

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0.expose().len())
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, HeaderValue)>,
    pub body: Option<Body>,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Request {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>, body: Secret) -> Self {
        Request {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body: Some(Body(body)),
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, HeaderValue::Public(value.into())));
        self
    }

    pub fn secret_header(mut self, name: &'static str, value: Secret) -> Self {
        self.headers.push((name, HeaderValue::Secret(value)));
        self
    }

    fn secrets(&self) -> impl Iterator<Item = &Secret> {
        self.headers.iter().filter_map(|(_, v)| match v {
            HeaderValue::Secret(s) => Some(s),
            HeaderValue::Public(_) => None,
        })
    }
}

/// A 2xx response; bodies are parsed, never logged.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Response {{ status: {}, body: <{} bytes> }}",
            self.status,
            self.body.len()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// Refused before any connection: not https, not an allowlisted host, or not a valid URL.
    Refused(String),
    /// A 3xx; redirects are never followed.
    Redirect(u16),
    /// A 4xx or 5xx with its body scrubbed of the credential.
    Status { status: u16, body: Scrubbed },
    /// Connection, TLS, timeout or read failure, scrubbed.
    Transport(Scrubbed),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Refused(why) => write!(f, "request refused: {why}"),
            HttpError::Redirect(s) => write!(f, "HTTP {s}: redirect refused"),
            HttpError::Status { status, body } => write!(f, "HTTP {status}: {body}"),
            HttpError::Transport(e) => write!(f, "network error: {e}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// What providers call. `credential` is the stored credential, used to scrub any error text.
pub trait Transport: Send + Sync {
    fn send(&self, request: &Request, credential: &Secret) -> Result<Response, HttpError>;
}

/// The bare wire: sends an already-vetted request and returns any status with its body.
pub trait RawTransport: Send + Sync {
    fn send_raw(&self, uri: &Uri, request: &Request) -> Result<Response, String>;
}

/// Checks a URL against the allowlist and returns the parsed form that is then sent as is.
pub fn vet_url(url: &str) -> Result<Uri, HttpError> {
    let uri: Uri = url
        .parse()
        .map_err(|_| HttpError::Refused("not a valid URL".into()))?;
    if uri.scheme_str() != Some("https") {
        return Err(HttpError::Refused("only https is allowed".into()));
    }
    let authority = uri
        .authority()
        .ok_or_else(|| HttpError::Refused("no host".into()))?;
    if authority.as_str().contains('@') {
        return Err(HttpError::Refused(
            "credentials in the URL are not allowed".into(),
        ));
    }
    if !matches!(authority.port_u16(), None | Some(443)) {
        return Err(HttpError::Refused(
            "only the default https port is allowed".into(),
        ));
    }
    let host = authority.host();
    if !ALLOWED_HOSTS.contains(&host) {
        return Err(HttpError::Refused(format!(
            "host {host} is not on the allowlist"
        )));
    }
    Ok(uri)
}

/// The policy layer: allowlist first, then the wire, then status classification and scrubbing.
pub struct Guarded<R> {
    raw: R,
}

impl<R: RawTransport> Guarded<R> {
    pub fn new(raw: R) -> Self {
        Guarded { raw }
    }
}

impl<R: RawTransport> Transport for Guarded<R> {
    fn send(&self, request: &Request, credential: &Secret) -> Result<Response, HttpError> {
        let uri = vet_url(&request.url)?;
        let mut secrets: Vec<&Secret> = request.secrets().collect();
        secrets.push(credential);
        let response = self
            .raw
            .send_raw(&uri, request)
            .map_err(|e| HttpError::Transport(scrub(&e, &secrets).truncated(MAX_ERROR_TEXT)))?;
        match response.status {
            200..=299 => Ok(response),
            300..=399 => Err(HttpError::Redirect(response.status)),
            status => {
                let text = String::from_utf8_lossy(&response.body);
                Err(HttpError::Status {
                    status,
                    body: scrub(&text, &secrets).truncated(MAX_ERROR_TEXT),
                })
            }
        }
    }
}

/// ureq 3 over rustls with redirects, env proxies and status-as-error all turned off.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .proxy(None)
            .user_agent(concat!("omadai/", env!("CARGO_PKG_VERSION")))
            .build();
        UreqTransport {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl UreqTransport {
    pub fn config(&self) -> &ureq::config::Config {
        self.agent.config()
    }
}

fn header_value(v: &HeaderValue) -> Result<ureq::http::HeaderValue, String> {
    let mut hv = ureq::http::HeaderValue::from_str(v.expose())
        .map_err(|_| "invalid header value".to_string())?;
    if matches!(v, HeaderValue::Secret(_)) {
        hv.set_sensitive(true);
    }
    Ok(hv)
}

impl RawTransport for UreqTransport {
    fn send_raw(&self, uri: &Uri, request: &Request) -> Result<Response, String> {
        let result = match request.method {
            Method::Get => {
                let mut b = self.agent.get(uri.clone());
                for (name, value) in &request.headers {
                    b = b.header(*name, header_value(value)?);
                }
                b.call()
            }
            Method::Post => {
                let mut b = self.agent.post(uri.clone());
                for (name, value) in &request.headers {
                    b = b.header(*name, header_value(value)?);
                }
                let body = request
                    .body
                    .as_ref()
                    .map(|b| b.0.expose().as_bytes())
                    .unwrap_or_default();
                b.send(body)
            }
        };
        let mut response = result.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        Ok(Response { status, body })
    }
}

/// A raw layer that never connects, so a test build of the binary cannot reach a provider.
#[cfg(feature = "test-fakes")]
pub struct Offline;

#[cfg(feature = "test-fakes")]
impl RawTransport for Offline {
    fn send_raw(&self, _uri: &Uri, _request: &Request) -> Result<Response, String> {
        Err("network disabled in this test build".into())
    }
}

/// The transport the product uses; a build with the `test-fakes` feature gets [`Offline`] behind the same guard.
pub fn default_transport() -> Box<dyn Transport> {
    #[cfg(feature = "test-fakes")]
    {
        Box::new(Guarded::new(Offline))
    }
    #[cfg(not(feature = "test-fakes"))]
    {
        Box::new(Guarded::new(UreqTransport::default()))
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// Replies with a fixed status and body, and records every URL it was asked to send.
    pub struct MockRaw {
        pub status: u16,
        pub body: Vec<u8>,
        pub sent: Mutex<Vec<String>>,
    }

    impl MockRaw {
        pub fn new(status: u16, body: &str) -> Self {
            MockRaw {
                status,
                body: body.as_bytes().to_vec(),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl RawTransport for &MockRaw {
        fn send_raw(&self, uri: &Uri, _request: &Request) -> Result<Response, String> {
            self.sent.lock().unwrap().push(uri.to_string());
            Ok(Response {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    /// One request as the wire saw it; header values are exposed, so only fake keys belong here.
    #[derive(Debug, Clone)]
    pub struct Sent {
        pub method: Method,
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: Option<String>,
    }

    impl Sent {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// A canned reply: URL pattern, status and body.
    type Reply = (String, u16, Vec<u8>);

    /// Answers each request with the first unused reply whose pattern the URL contains, in order.
    pub struct MockSeq {
        replies: Mutex<Vec<Option<Reply>>>,
        pub sent: Mutex<Vec<Sent>>,
    }

    impl MockSeq {
        pub fn new(replies: Vec<(&str, u16, String)>) -> Self {
            MockSeq {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(p, s, b)| Some((p.to_string(), s, b.into_bytes())))
                        .collect(),
                ),
                sent: Mutex::new(Vec::new()),
            }
        }

        pub fn sent(&self) -> Vec<Sent> {
            self.sent.lock().unwrap().clone()
        }

        /// Replies not yet used, by pattern.
        pub fn unused(&self) -> Vec<String> {
            self.replies
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .map(|(p, _, _)| p.clone())
                .collect()
        }
    }

    impl RawTransport for &MockSeq {
        fn send_raw(&self, uri: &Uri, request: &Request) -> Result<Response, String> {
            let url = uri.to_string();
            self.sent.lock().unwrap().push(Sent {
                method: request.method,
                url: url.clone(),
                headers: request
                    .headers
                    .iter()
                    .map(|(n, v)| (n.to_string(), v.expose().to_string()))
                    .collect(),
                body: request.body.as_ref().map(|b| b.0.expose().to_string()),
            });
            let mut replies = self.replies.lock().unwrap();
            let slot = replies
                .iter_mut()
                .find(|r| r.as_ref().is_some_and(|(p, _, _)| url.contains(p.as_str())))
                .ok_or_else(|| format!("no mock reply for {url}"))?;
            let (_, status, body) = slot.take().unwrap();
            Ok(Response { status, body })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockRaw;
    use super::*;

    const KEY: &str = "fk-admin-SUPERSECRET0123456789";

    fn key() -> Secret {
        Secret::new(KEY.into())
    }

    #[test]
    fn refuses_off_allowlist_before_sending() {
        let raw = MockRaw::new(200, "{}");
        let t = Guarded::new(&raw);
        for url in [
            "http://api.openai.com/v1/x",
            "https://evil.example/v1",
            "https://api.openai.com.evil.example/v1",
            "https://api.openai.com:8443/v1",
            "https://api.openai.com@evil.example/v1",
            "https://API.OPENAI.COM/v1",
            "https://127.0.0.1/v1",
            "api.openai.com/v1",
            "ftp://api.openai.com/",
        ] {
            let r = t.send(&Request::get(url), &key());
            assert!(
                matches!(r, Err(HttpError::Refused(_))),
                "{url} not refused: {r:?}"
            );
        }
        assert!(
            raw.sent.lock().unwrap().is_empty(),
            "a refused request reached the wire"
        );
        assert!(
            t.send(
                &Request::get("https://api.openai.com/v1/organization/costs"),
                &key()
            )
            .is_ok()
        );
        assert!(
            t.send(&Request::get("https://api.openai.com:443/v1"), &key())
                .is_ok()
        );
    }

    #[test]
    fn redirect_is_an_error() {
        for status in [301, 302, 303, 307, 308] {
            let raw = MockRaw::new(status, "");
            let r = Guarded::new(&raw)
                .send(&Request::get("https://api.fal.ai/v1/models/usage"), &key());
            assert_eq!(r.unwrap_err(), HttpError::Redirect(status));
        }
    }

    #[test]
    fn error_body_with_key_is_scrubbed() {
        let body = format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {KEY}. Starts with fk-admin"}}}}"#
        );
        let raw = MockRaw::new(401, &body);
        let req = Request::get("https://api.openai.com/v1/organization/costs")
            .secret_header("Authorization", Secret::with_prefix("Bearer ", &key()));
        let err = Guarded::new(&raw).send(&req, &key()).unwrap_err();
        let shown = err.to_string();
        let HttpError::Status {
            status,
            body: stored,
        } = &err
        else {
            panic!("{err:?}")
        };
        assert_eq!(*status, 401);
        for text in [
            shown.as_str(),
            stored.as_str(),
            &format!("{err:?}"),
            &serde_json::to_string(stored).unwrap(),
        ] {
            assert!(!text.contains(KEY), "key leaked: {text}");
            assert!(!text.contains("fk-admin"), "key prefix leaked: {text}");
        }
        assert!(
            shown.contains("Incorrect API key provided: [redacted]"),
            "{shown}"
        );
    }

    #[test]
    fn long_error_body_is_scrubbed_then_capped() {
        // The key straddles the cap, so cutting before scrubbing would keep a fragment of it.
        let body = format!(
            "{}{KEY}{}",
            "x".repeat(MAX_ERROR_TEXT - 12),
            "€".repeat(4000)
        );
        let raw = MockRaw::new(500, &body);
        let err = Guarded::new(&raw)
            .send(&Request::get("https://api.openai.com/v1/x"), &key())
            .unwrap_err();
        let HttpError::Status { body: stored, .. } = &err else {
            panic!("{err:?}")
        };
        let text = stored.as_str();
        assert!(
            text.len() <= MAX_ERROR_TEXT + crate::scrub::TRUNCATED.len(),
            "{}",
            text.len()
        );
        assert!(text.ends_with(crate::scrub::TRUNCATED), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert!(!text.contains("fk-a") && !text.contains("SUPER"), "{text}");
        assert!(err.to_string().len() < MAX_ERROR_TEXT + 64);
    }

    #[test]
    fn request_debug_hides_secrets() {
        let req = Request::post(
            "https://oauth2.googleapis.com/token",
            Secret::new(format!("refresh_token={KEY}")),
        )
        .header("content-type", "application/x-www-form-urlencoded")
        .secret_header("Authorization", Secret::with_prefix("Bearer ", &key()));
        for text in [format!("{req:?}"), format!("{req:#?}")] {
            assert!(!text.contains(KEY), "{text}");
            assert!(!text.contains("SUPERSECRET"), "{text}");
        }
        let resp = Response {
            status: 200,
            body: KEY.as_bytes().to_vec(),
        };
        assert!(!format!("{resp:?}").contains(KEY));
    }

    #[test]
    fn ureq_agent_is_locked_down() {
        let t = UreqTransport::default();
        let c = t.config();
        assert_eq!(c.max_redirects(), 0);
        assert!(c.https_only());
        assert!(!c.http_status_as_error());
        assert!(c.proxy().is_none());
        assert_eq!(c.timeouts().global, Some(TIMEOUT));
    }
}
