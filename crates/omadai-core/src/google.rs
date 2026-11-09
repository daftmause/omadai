//! Google sign-in for Gemini: PKCE, the loopback redirect, the token exchange and refresh, and the
//! BigQuery discovery `key add` runs once signed in.

use crate::http::{HttpError, Request, Transport};
use crate::provider::{Cause, ProviderError};
use crate::providers::gemini::bigquery_error;
use crate::providers::{
    CallError, bearer, call_json, enc, enc_into, enc_len, fail, from_http, json_strings, paged,
};
use crate::scrub::plain;
use crate::secret::Secret;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
/// The narrowest read-only scope BigQuery v2 lists for `jobs.query`, `datasets.get` and `tables.list`.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform.read-only";
const BIGQUERY: &str = "https://bigquery.googleapis.com/bigquery/v2/projects";
/// The table name prefix the standard usage cost export writes.
pub const EXPORT_PREFIX: &str = "gcp_billing_export_v1_";

/// How long `key add` waits for the browser to come back.
pub const SIGN_IN_WAIT: Duration = Duration::from_secs(5 * 60);
/// The longest one connection may take to send its request line.
pub const READ_CAP: Duration = Duration::from_secs(10);
/// The longest request line read.
pub const MAX_LINE: usize = 8 * 1024;

pub const SIGNED_IN_PAGE: &str = "omadai: signed in, return to the terminal\n";
pub const REFUSED_PAGE: &str = "omadai: Google refused the sign-in, return to the terminal\n";
pub const BAD_REQUEST_PAGE: &str = "omadai: not a sign-in reply\n";

pub const REVOKE_HINT: &str =
    "revoke omadai's access at myaccount.google.com/permissions and sign in again";

/// base64url without padding (RFC 4648 section 5).
pub fn base64url(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// The S256 code challenge for a verifier (RFC 7636 section 4.2).
pub fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

fn random(n: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut bytes = Zeroizing::new(vec![0u8; n]);
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("could not read /dev/urandom: {e}"))?;
    Ok(bytes)
}

/// One sign-in's PKCE verifier, its challenge and the `state` value.
pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn new() -> Result<Self, String> {
        let verifier = Secret::new(base64url(&random(32)?));
        Ok(Pkce {
            challenge: challenge(verifier.expose()),
            verifier,
            state: base64url(&random(16)?),
        })
    }
}

pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The consent URL; it holds the public client ID, the challenge and `state`, never a secret.
pub fn auth_url(client_id: &str, port: u16, challenge: &str, state: &str) -> String {
    let params = [
        ("client_id", client_id),
        ("redirect_uri", &redirect_uri(port)),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ];
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect();
    format!("{AUTH_URL}?{}", query.join("&"))
}

/// The local listener Google's redirect comes back to.
pub struct Loopback {
    listener: TcpListener,
    /// The overall wait; `SIGN_IN_WAIT` in use.
    pub wait: Duration,
    /// The longest one connection may take; `READ_CAP` in use.
    pub read_cap: Duration,
}

enum Served {
    Code(Secret),
    Refused(String),
    Bad,
}

impl Loopback {
    pub fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|l| l.set_nonblocking(true).map(|()| l))
            .map_err(|e| format!("could not listen on 127.0.0.1 for the sign-in: {e}"))?;
        Ok(Loopback {
            listener,
            wait: SIGN_IN_WAIT,
            read_cap: READ_CAP,
        })
    }

    pub fn port(&self) -> Result<u16, String> {
        self.listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| format!("could not read the sign-in port: {e}"))
    }

    /// Waits for the redirect carrying `state` and returns its authorization code.
    pub fn wait_for_code(&self, state: &str) -> Result<Secret, String> {
        let deadline = Instant::now() + self.wait;
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => match self.serve(stream, peer, state, deadline) {
                    Served::Code(code) => return Ok(code),
                    Served::Refused(name) => {
                        return Err(format!("Google refused the sign-in: {name}"));
                    }
                    Served::Bad => {}
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                // A connection reset before it was accepted, or the like, is skipped.
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "no sign-in arrived within {} s; run key add again",
                    self.wait.as_secs()
                ));
            }
        }
    }

    fn serve(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        state: &str,
        deadline: Instant,
    ) -> Served {
        let until = deadline.min(Instant::now() + self.read_cap);
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero()
            || stream.set_nonblocking(false).is_err()
            || stream.set_write_timeout(Some(left)).is_err()
        {
            return Served::Bad;
        }
        let served = match read_line(&mut stream, until) {
            Some(line) if peer.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) => judge(&line, state),
            _ => Served::Bad,
        };
        let (status, page) = match &served {
            Served::Code(_) => ("200 OK", SIGNED_IN_PAGE),
            Served::Refused(_) => ("200 OK", REFUSED_PAGE),
            Served::Bad => ("400 Bad Request", BAD_REQUEST_PAGE),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.flush();
        // Reading what is left before closing keeps the close from resetting the reply away.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        // The drain has its own 200 ms bound, so a peer that keeps sending cannot stretch it.
        let drain = Instant::now() + Duration::from_millis(200);
        let mut rest = Zeroizing::new([0u8; 1024]);
        loop {
            let left = drain.saturating_duration_since(Instant::now());
            if left.is_zero()
                || stream.set_read_timeout(Some(left)).is_err()
                || !matches!(stream.read(&mut rest[..]), Ok(n) if n > 0)
            {
                break;
            }
        }
        served
    }
}

/// The request line without its line ending, or `None` past `MAX_LINE`, past `until` or on non-UTF-8.
fn read_line(stream: &mut TcpStream, until: Instant) -> Option<Zeroizing<String>> {
    let mut line = Zeroizing::new(Vec::with_capacity(MAX_LINE + 1024));
    let mut buf = Zeroizing::new([0u8; 1024]);
    loop {
        // Each read waits only for what is left, so a peer trickling bytes cannot hold the listener.
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return None;
        }
        let n = stream.read(&mut buf[..]).ok()?;
        if n == 0 {
            return None;
        }
        line.extend_from_slice(&buf[..n]);
        if let Some(end) = line.iter().position(|b| *b == b'\n') {
            if end > MAX_LINE {
                return None;
            }
            line.truncate(end);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return copy_utf8(&line);
        }
        if line.len() > MAX_LINE {
            return None;
        }
    }
}

fn judge(line: &str, state: &str) -> Served {
    let mut words = line.split(' ');
    let (Some("GET"), Some(target), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Served::Bad;
    };
    let Some(query) = target.strip_prefix("/?") else {
        return Served::Bad;
    };
    if !version.starts_with("HTTP/1.") {
        return Served::Bad;
    }
    let (mut got_state, mut code, mut error) = (None, None, None);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(v) = decode(v) else {
            return Served::Bad;
        };
        match k {
            "state" => got_state = Some(v),
            "code" => code = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }
    if !got_state.is_some_and(|s| same(s.as_bytes(), state.as_bytes())) {
        return Served::Bad;
    }
    if let Some(e) = error {
        // Only an OAuth error code's characters are kept, so the reply cannot carry arbitrary text.
        let name: String = e
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '_')
            .take(64)
            .collect();
        return Served::Refused(if name.is_empty() {
            "unknown".into()
        } else {
            name
        });
    }
    match code {
        Some(mut c) if !c.is_empty() => Served::Code(Secret::new(std::mem::take(&mut *c))),
        _ => Served::Bad,
    }
}

/// Percent-decodes a query value, `+` as a space, into a zeroizing buffer.
fn decode(v: &str) -> Option<Zeroizing<String>> {
    let b = v.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(b.len()));
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    copy_utf8(&out)
}

/// Checks UTF-8 in place and copies into a zeroizing string, so no unzeroized buffer is left behind.
fn copy_utf8(bytes: &[u8]) -> Option<Zeroizing<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = Zeroizing::new(String::with_capacity(text.len()));
    out.push_str(text);
    Some(out)
}

/// Compares in time that depends only on the lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// A form-encoded body built in one buffer sized up front, so no reallocation leaves a copy.
fn form(pairs: &[(&str, &str)]) -> Secret {
    let len = pairs
        .iter()
        .map(|(k, v)| k.len() + 1 + enc_len(v))
        .sum::<usize>()
        + pairs.len().saturating_sub(1);
    let mut out = String::with_capacity(len);
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        enc_into(&mut out, v);
    }
    Secret::new(out)
}

/// `credential` plus the percent-encoded form of each `:` part, the form `form` sends, as scrub needles.
fn with_encoded(credential: &Secret) -> Secret {
    let len = credential.expose().len() + credential.parts().map(|p| 1 + enc_len(p)).sum::<usize>();
    let mut out = String::with_capacity(len);
    out.push_str(credential.expose());
    for part in credential.parts() {
        out.push(':');
        enc_into(&mut out, part);
    }
    Secret::new(out)
}

/// A token string read straight into a `Secret`.
struct SecretField(Secret);

impl<'de> Deserialize<'de> for SecretField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(|s| SecretField(Secret::new(s)))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<SecretField>,
    #[serde(default)]
    refresh_token: Option<SecretField>,
    /// Space-separated; RFC 6749 lets it be absent when it equals the request.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    #[serde(default)]
    error_subtype: Option<String>,
}

/// What the code exchange returns.
pub struct Tokens {
    pub refresh: Secret,
    pub access: Secret,
}

fn token_post(
    http: &dyn Transport,
    body: Secret,
    credential: &Secret,
) -> Result<TokenResponse, CallError> {
    let req =
        Request::post(TOKEN_URL, body).header("content-type", "application/x-www-form-urlencoded");
    let mut response = http.send(&req, credential).map_err(CallError::Http)?;
    // A parse error can quote the body, which holds tokens, so its text is never shown.
    let parsed = serde_json::from_slice::<TokenResponse>(&response.body);
    response.body.zeroize();
    parsed.map_err(|_| CallError::Parse(plain("unexpected token response from Google")))
}

/// Exchanges the authorization code; `credential` joins every value sent, so an echo is scrubbed.
pub fn exchange(
    http: &dyn Transport,
    client_id: &Secret,
    client_secret: &Secret,
    code: &Secret,
    verifier: &Secret,
    redirect_uri: &str,
) -> Result<Tokens, ProviderError> {
    let credential = with_encoded(&Secret::join(&[client_id, client_secret, code, verifier]));
    let body = form(&[
        ("code", code.expose()),
        ("code_verifier", verifier.expose()),
        ("client_id", client_id.expose()),
        ("client_secret", client_secret.expose()),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ]);
    let tokens = token_post(http, body, &credential).map_err(|e| match e {
        // A 200 that does not parse may still have issued tokens.
        CallError::Parse(m) => fail(format!("{m}; {REVOKE_HINT}")),
        e => e.into_provider(&credential),
    })?;
    // With granular consent a person can leave the scope unticked, which every later query would refuse.
    if tokens
        .scope
        .as_deref()
        .is_some_and(|granted| !granted.split(' ').any(|g| g == SCOPE))
    {
        return Err(fail(format!(
            "the Google sign-in did not grant read access to Cloud resources, which omadai needs; {REVOKE_HINT}"
        )));
    }
    match (tokens.refresh_token, tokens.access_token) {
        (Some(SecretField(refresh)), Some(SecretField(access)))
            if !refresh.is_empty() && !access.is_empty() =>
        {
            if refresh.expose().contains(':') {
                return Err(fail(format!(
                    "Google returned a refresh token omadai cannot store; {REVOKE_HINT}"
                )));
            }
            Ok(Tokens { refresh, access })
        }
        _ => Err(fail(format!(
            "Google returned no refresh token; {REVOKE_HINT}"
        ))),
    }
}

/// A fresh access token from the stored `client_id:client_secret:refresh_token`.
pub fn access_token(http: &dyn Transport, stored: &Secret) -> Result<Secret, ProviderError> {
    let parts: Vec<&str> = stored.parts().collect();
    let [client_id, client_secret, refresh] = parts[..] else {
        return Err(fail(
            "the stored Gemini credential is not a Google sign-in; remove the Gemini key and add it again",
        ));
    };
    if client_id.is_empty() || client_secret.is_empty() || refresh.is_empty() {
        return Err(fail(
            "the stored Gemini credential is incomplete; remove the Gemini key and add it again",
        ));
    }
    let body = form(&[
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh),
        ("grant_type", "refresh_token"),
    ]);
    let stored = &with_encoded(stored);
    match token_post(http, body, stored) {
        Ok(TokenResponse {
            access_token: Some(SecretField(access)),
            ..
        }) if !access.is_empty() => Ok(access),
        Ok(_) => Err(fail("Google returned no access token")),
        Err(CallError::Parse(m)) => Err(CallError::Parse(m).into_provider(stored)),
        Err(CallError::Http(e)) => {
            let mut out = from_http(&e, stored);
            if let HttpError::Status {
                status: 400 | 401,
                body,
            } = &e
            {
                // A body cut to `MAX_ERROR_TEXT` no longer parses, so the code is scanned for.
                let (codes, subtypes) = serde_json::from_str::<OAuthError>(body.as_str())
                    .map_or_else(
                        |_| {
                            let mut subtypes = json_strings(body.as_str(), "error_subtype");
                            // Google sends `error_subtype` last, so a cut body may hold it only in the
                            // description, as in "reauth related error (invalid_rapt)".
                            if body.as_str().contains("(invalid_rapt)") {
                                subtypes.push("invalid_rapt".into());
                            }
                            (json_strings(body.as_str(), "error"), subtypes)
                        },
                        |o| (vec![o.error], o.error_subtype.into_iter().collect()),
                    );
                if codes.iter().any(|c| c == "invalid_grant") {
                    // `invalid_rapt` is a Workspace session-control timer, not a revoked sign-in.
                    out.cause = if subtypes.iter().any(|c| c == "invalid_rapt") {
                        Cause::SessionPolicy
                    } else {
                        Cause::SignInExpired
                    };
                } else if codes.iter().any(|c| c == "invalid_client") {
                    out.cause = Cause::ClientGone;
                }
            }
            Err(out)
        }
    }
}

/// What a revoke achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    /// Google revoked the grant behind the token.
    Done,
    /// Google answered `invalid_token`: it held no live grant for it, so nothing changed.
    AlreadyGone,
}

/// Revokes `token` and the grant behind it.
pub fn revoke(
    http: &dyn Transport,
    token: &Secret,
    scrub: &Secret,
) -> Result<Revoked, ProviderError> {
    // The token goes in the form body, never the URL.
    let req = Request::post(REVOKE_URL, form(&[("token", token.expose())]))
        .header("content-type", "application/x-www-form-urlencoded");
    let scrub = &with_encoded(&Secret::join(&[scrub, token]));
    match http.send(&req, scrub) {
        Ok(mut response) => {
            response.body.zeroize();
            Ok(Revoked::Done)
        }
        Err(e) => {
            if let HttpError::Status { status: 400, body } = &e {
                // A body cut to `MAX_ERROR_TEXT` no longer parses, so the code is scanned for.
                let codes = serde_json::from_str::<OAuthError>(body.as_str())
                    .map_or_else(|_| json_strings(body.as_str(), "error"), |o| vec![o.error]);
                if codes.iter().any(|c| c == "invalid_token") {
                    return Ok(Revoked::AlreadyGone);
                }
            }
            Err(from_http(&e, scrub))
        }
    }
}

/// Revokes the refresh token in a stored `client_id:client_secret:refresh_token`.
pub fn revoke_stored(http: &dyn Transport, stored: &Secret) -> Result<Revoked, ProviderError> {
    let parts: Vec<&str> = stored.parts().collect();
    let [_, _, refresh] = parts[..] else {
        return Err(fail("the stored Gemini credential is not a Google sign-in"));
    };
    if refresh.is_empty() {
        return Err(fail("the stored Gemini credential holds no refresh token"));
    }
    revoke(http, &Secret::new(refresh.to_string()), stored)
}

#[derive(Deserialize)]
struct Dataset {
    location: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableList {
    #[serde(default)]
    tables: Vec<Table>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Table {
    table_reference: TableRef,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableRef {
    table_id: String,
}

fn discover<T: serde::de::DeserializeOwned>(
    http: &dyn Transport,
    access: &Secret,
    scrub: &Secret,
    url: String,
    project: &str,
    dataset: &str,
) -> Result<T, ProviderError> {
    let req = Request::get(url).secret_header("Authorization", bearer(access));
    // The bare access token is a needle too: the header only scrubs it with its `Bearer ` prefix.
    let scrub = Secret::join(&[scrub, access]);
    call_json(http, &req, &scrub).map_err(|e: CallError| {
        bigquery_error(
            e,
            &scrub,
            project,
            dataset,
            format!("there is no dataset {dataset} in project {project}"),
        )
    })
}

/// The dataset's BigQuery location (`datasets.get`).
pub fn dataset_location(
    http: &dyn Transport,
    access: &Secret,
    scrub: &Secret,
    project: &str,
    dataset: &str,
) -> Result<String, ProviderError> {
    let url = format!("{BIGQUERY}/{}/datasets/{}", enc(project), enc(dataset));
    let d: Dataset = discover(http, access, scrub, url, project, dataset)?;
    Ok(d.location)
}

/// The standard usage cost export tables, following `nextPageToken` through every page (at most `MAX_PAGES`).
pub fn export_tables(
    http: &dyn Transport,
    access: &Secret,
    scrub: &Secret,
    project: &str,
    dataset: &str,
) -> Result<Vec<String>, ProviderError> {
    let base = format!(
        "{BIGQUERY}/{}/datasets/{}/tables?maxResults=1000",
        enc(project),
        enc(dataset)
    );
    let mut found = Vec::new();
    paged(
        |cursor| {
            let url = match cursor {
                Some(c) => format!("{base}&pageToken={}", enc(c)),
                None => base.clone(),
            };
            discover::<TableList>(http, access, scrub, url, project, dataset)
        },
        |list| Ok(list.next_page_token.clone().filter(|t| !t.is_empty())),
        |list| {
            found.extend(
                list.tables
                    .into_iter()
                    .map(|t| t.table_reference.table_id)
                    .filter(|id| id.starts_with(EXPORT_PREFIX)),
            );
            Ok(())
        },
    )?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockSeq;
    use crate::providers::testutil::{assert_no_key, guarded};

    #[test]
    fn rfc_7636_appendix_b_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        let p = Pkce::new().unwrap();
        assert_eq!(p.verifier.expose().len(), 43);
        assert_eq!(p.state.len(), 22);
        assert_eq!(p.challenge, challenge(p.verifier.expose()));
    }

    #[test]
    fn auth_url_carries_every_parameter_encoded() {
        let url = auth_url("fk-client id&x", 4242, "chal-_", "st/ate");
        let (base, query) = url.split_once('?').unwrap();
        assert_eq!(base, AUTH_URL);
        let params: Vec<&str> = query.split('&').collect();
        assert_eq!(
            params,
            [
                "client_id=fk-client%20id%26x",
                "redirect_uri=http%3A%2F%2F127.0.0.1%3A4242",
                "response_type=code",
                "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform.read-only",
                "code_challenge=chal-_",
                "code_challenge_method=S256",
                "state=st%2Fate",
                "access_type=offline",
                "prompt=consent",
            ]
        );
    }

    const STATE: &str = "fk-state-0123456789";

    fn loopback(wait_ms: u64, cap_ms: u64) -> (Loopback, u16) {
        let mut l = Loopback::bind().unwrap();
        l.wait = Duration::from_millis(wait_ms);
        l.read_cap = Duration::from_millis(cap_ms);
        let port = l.port().unwrap();
        (l, port)
    }

    /// Sends `raw` and returns the whole reply.
    fn send(port: u16, raw: &[u8]) -> String {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s.write_all(raw).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn get(query: &str) -> Vec<u8> {
        format!("GET /?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").into_bytes()
    }

    #[test]
    fn loopback_turns_away_bad_requests_and_returns_the_right_code() {
        let (l, port) = loopback(3000, 300);
        let oversized = get(&format!("state={STATE}&code={}", "a".repeat(MAX_LINE)));
        let bad: Vec<Vec<u8>> = vec![
            get("state=fk-wrong-state&code=fk-code-WRONGSTATE"),
            get(&format!("state={STATE}")),
            format!("POST /?state={STATE}&code=fk-code-POSTED HTTP/1.1\r\n\r\n").into_bytes(),
            format!("GET /other?state={STATE}&code=fk-code-PATH HTTP/1.1\r\n\r\n").into_bytes(),
            oversized,
        ];
        let client = std::thread::spawn(move || {
            let mut replies = Vec::new();
            for raw in &bad {
                replies.push((raw.clone(), send(port, raw)));
            }
            // A silent peer is dropped after the read cap and the wait goes on.
            let silent = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            let start = Instant::now();
            let mut silent_reply = String::new();
            let _ = (&silent).read_to_string(&mut silent_reply);
            let waited = start.elapsed();
            drop(silent);
            let ok = send(port, &get(&format!("state={STATE}&code=fk-code%2FRIGHT")));
            (replies, silent_reply, waited, ok)
        });
        let code = l.wait_for_code(STATE).unwrap();
        assert_eq!(code.expose(), "fk-code/RIGHT");
        let (replies, silent_reply, waited, ok) = client.join().unwrap();
        for (raw, reply) in &replies {
            let line = String::from_utf8_lossy(raw);
            let line = line.lines().next().unwrap();
            assert!(reply.starts_with("HTTP/1.1 400 "), "{line}: {reply}");
            assert!(reply.ends_with(BAD_REQUEST_PAGE), "{reply}");
            assert!(
                !reply.contains(line),
                "the reply echoes the request: {reply}"
            );
            assert!(!reply.contains("fk-code"), "{reply}");
        }
        assert!(silent_reply.starts_with("HTTP/1.1 400 "), "{silent_reply}");
        assert!(waited >= Duration::from_millis(250), "{waited:?}");
        assert!(
            ok.starts_with("HTTP/1.1 200 ") && ok.ends_with(SIGNED_IN_PAGE),
            "{ok}"
        );
    }

    #[test]
    fn loopback_ends_on_googles_refusal_with_its_error_name() {
        let (l, port) = loopback(3000, 300);
        let client = std::thread::spawn(move || {
            send(port, &get(&format!("error=access_denied&state={STATE}")))
        });
        let e = l.wait_for_code(STATE).unwrap_err();
        assert_eq!(e, "Google refused the sign-in: access_denied");
        assert!(client.join().unwrap().ends_with(REFUSED_PAGE));
        // A refusal under another state is not Google's, so it is turned away.
        let (l, port) = loopback(600, 200);
        let client =
            std::thread::spawn(move || send(port, &get("error=access_denied&state=fk-other")));
        let e = l.wait_for_code(STATE).unwrap_err();
        assert!(e.starts_with("no sign-in arrived"), "{e}");
        assert!(client.join().unwrap().starts_with("HTTP/1.1 400 "));
    }

    #[test]
    fn loopback_gives_up_at_the_deadline() {
        let (l, _port) = loopback(300, 100);
        let start = Instant::now();
        let e = l.wait_for_code(STATE).unwrap_err();
        assert!(e.starts_with("no sign-in arrived"), "{e}");
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    const CLIENT_ID: &str = "fk-clientid-0123456789.apps.example";
    const CLIENT_SECRET: &str = "fk-clientsecret-ABCDEFGHIJ";
    const CODE: &str = "fk-authcode-KLMNOPQRSTUV";
    const VERIFIER: &str = "fk-verifier-WXYZ0123456789abcdefghijklmnop";
    const REFRESH: &str = "fk-refresh-1234567890ABCDEFGH";

    fn s(v: &str) -> Secret {
        Secret::new(v.into())
    }

    #[test]
    fn exchange_sends_the_form_and_keeps_the_tokens_secret() {
        let mock = MockSeq::new(vec![(
            "/token",
            200,
            include_str!("../fixtures/gemini/token_exchange.json").into(),
        )]);
        let t = exchange(
            &guarded(&mock),
            &s(CLIENT_ID),
            &s(CLIENT_SECRET),
            &s(CODE),
            &s(VERIFIER),
            "http://127.0.0.1:4242",
        )
        .unwrap();
        assert!(t.refresh.expose().starts_with("fk-"));
        assert!(t.access.expose().starts_with("fk-"));
        let sent = mock.sent();
        assert_eq!(sent[0].url, TOKEN_URL);
        assert_eq!(
            sent[0].body.as_deref().unwrap(),
            format!(
                "code={CODE}&code_verifier={VERIFIER}&client_id={CLIENT_ID}&client_secret={CLIENT_SECRET}\
                 &redirect_uri=http%3A%2F%2F127.0.0.1%3A4242&grant_type=authorization_code"
            )
        );
        // A grant without the scope is refused before anything is stored.
        let narrow = include_str!("../fixtures/gemini/token_exchange.json")
            .replace(SCOPE, "https://www.googleapis.com/auth/userinfo.email");
        let mock = MockSeq::new(vec![("/token", 200, narrow)]);
        let Err(e) = exchange(
            &guarded(&mock),
            &s(CLIENT_ID),
            &s(CLIENT_SECRET),
            &s(CODE),
            &s(VERIFIER),
            "http://127.0.0.1:4242",
        ) else {
            panic!("a grant without the scope was accepted");
        };
        assert!(e.message.as_str().contains("did not grant"), "{e:?}");
        assert!(e.message.as_str().contains(REVOKE_HINT), "{e:?}");
        // No refresh token: the person is told how to get one issued again.
        let mock = MockSeq::new(vec![("/token", 200, r#"{"access_token":"fk-a-1"}"#.into())]);
        let Err(e) = exchange(
            &guarded(&mock),
            &s(CLIENT_ID),
            &s(CLIENT_SECRET),
            &s(CODE),
            &s(VERIFIER),
            "http://127.0.0.1:1",
        ) else {
            panic!("an exchange without a refresh token succeeded")
        };
        assert!(
            e.message
                .as_str()
                .contains("myaccount.google.com/permissions"),
            "{e:?}"
        );
    }

    #[test]
    fn an_exchange_error_echoing_every_value_leaves_no_trace() {
        let body = format!(
            r#"{{"error":"invalid_request","error_description":"code {CODE} verifier {VERIFIER} secret {CLIENT_SECRET}"}}"#
        );
        let mock = MockSeq::new(vec![("/token", 400, body)]);
        let Err(e) = exchange(
            &guarded(&mock),
            &s(CLIENT_ID),
            &s(CLIENT_SECRET),
            &s(CODE),
            &s(VERIFIER),
            "http://127.0.0.1:1",
        ) else {
            panic!("the exchange succeeded")
        };
        assert_eq!(e.cause, Cause::Status(400));
        for key in [CODE, VERIFIER, CLIENT_SECRET] {
            assert_no_key(&e, key);
        }
    }

    #[test]
    fn a_refused_refresh_is_sign_in_expired_and_leaves_no_trace() {
        let stored = Secret::join(&[&s(CLIENT_ID), &s(CLIENT_SECRET), &s(REFRESH)]);
        let body = format!(
            r#"{{"error":"invalid_grant","error_description":"Token {REFRESH} has been expired or revoked."}}"#
        );
        let mock = MockSeq::new(vec![("/token", 400, body)]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::SignInExpired);
        assert_no_key(&e, REFRESH);
        assert_no_key(&e, CLIENT_SECRET);
        assert_eq!(
            mock.sent()[0].body.as_deref().unwrap(),
            format!(
                "client_id={CLIENT_ID}&client_secret={CLIENT_SECRET}&refresh_token={REFRESH}&grant_type=refresh_token"
            )
        );
        // A deleted or rotated OAuth client, answered 400 or 401, has its own cause.
        for status in [400, 401] {
            let mock = MockSeq::new(vec![(
                "/token",
                status,
                r#"{"error":"invalid_client","error_description":"The OAuth client was not found."}"#
                    .into(),
            )]);
            let e = access_token(&guarded(&mock), &stored).unwrap_err();
            assert_eq!(e.cause, Cause::ClientGone, "{status}");
        }
        // Any other 400 keeps its status.
        let mock = MockSeq::new(vec![(
            "/token",
            400,
            r#"{"error":"unsupported_grant_type"}"#.into(),
        )]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::Status(400));
        let mock = MockSeq::new(vec![(
            "/token",
            200,
            include_str!("../fixtures/gemini/token_refresh.json").into(),
        )]);
        assert!(
            access_token(&guarded(&mock), &stored)
                .unwrap()
                .expose()
                .starts_with("fk-")
        );
        // A body cut short by `MAX_ERROR_TEXT` still names invalid_grant.
        let long = format!(
            r#"{{"error": "invalid_grant", "error_description": "{}"}}"#,
            "x".repeat(4096)
        );
        let mock = MockSeq::new(vec![("/token", 400, long)]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::SignInExpired);
        // Workspace's Google Cloud session control answers invalid_grant with subtype invalid_rapt.
        let rapt = r#"{"error": "invalid_grant", "error_description": "reauth related error (invalid_rapt)", "error_uri": "https://support.google.com/a/answer/9368756", "error_subtype": "invalid_rapt"}"#;
        let mock = MockSeq::new(vec![("/token", 400, rapt.into())]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::SessionPolicy);
        // A cut-short body still names the subtype.
        let long = format!(
            r#"{{"error": "invalid_grant", "error_subtype": "invalid_rapt", "error_description": "{}"}}"#,
            "x".repeat(4096)
        );
        let mock = MockSeq::new(vec![("/token", 400, long)]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::SessionPolicy);
        // Google's own field order puts error_subtype last, so a cut body loses it; the
        // description still names invalid_rapt.
        let cut = format!(
            r#"{{"error": "invalid_grant", "error_description": "reauth related error (invalid_rapt){}", "error_uri": "https://support.google.com/a/answer/9368756", "error_subtype": "invalid_rapt"}}"#,
            " ".repeat(4096)
        );
        let mock = MockSeq::new(vec![("/token", 400, cut)]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::SessionPolicy);
        // Another subtype is still an expired sign-in.
        let other = r#"{"error": "invalid_grant", "error_subtype": "something_else"}"#;
        let mock = MockSeq::new(vec![("/token", 400, other.into())]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        assert_eq!(e.cause, Cause::SignInExpired);
        // A malformed stored value is refused before any request.
        let mock = MockSeq::new(vec![]);
        assert!(access_token(&guarded(&mock), &s("fk-only-one-part")).is_err());
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn a_refresh_token_echoed_percent_encoded_leaves_no_trace() {
        const SLASHED: &str = "fk-1//0refresh-SLASH0123456789";
        let stored = Secret::join(&[&s(CLIENT_ID), &s(CLIENT_SECRET), &s(SLASHED)]);
        let body = r#"{"error":"invalid_request","error_description":"bad refresh_token=fk-1%2F%2F0refresh-SLASH0123456789"}"#;
        let mock = MockSeq::new(vec![("/token", 400, body.into())]);
        let e = access_token(&guarded(&mock), &stored).unwrap_err();
        for text in [e.message.to_string(), format!("{e:?}")] {
            assert!(!text.contains("SLASH0123456789"), "{text}");
        }
    }

    #[test]
    fn revoke_sends_the_token_in_the_body_only() {
        let mock = MockSeq::new(vec![("/revoke", 200, "{}".into())]);
        assert_eq!(
            revoke(&guarded(&mock), &s(REFRESH), &s(CLIENT_SECRET)).unwrap(),
            Revoked::Done
        );
        let sent = mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, crate::http::Method::Post);
        assert_eq!(sent[0].url, REVOKE_URL);
        assert!(!sent[0].url.contains('?'));
        assert_eq!(sent[0].body.as_deref().unwrap(), format!("token={REFRESH}"));
        assert_eq!(
            sent[0].header("content-type"),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn a_token_google_no_longer_knows_is_already_gone() {
        let mock = MockSeq::new(vec![(
            "/revoke",
            400,
            r#"{"error": "invalid_token"}"#.into(),
        )]);
        assert_eq!(
            revoke(&guarded(&mock), &s(REFRESH), &s(CLIENT_SECRET)).unwrap(),
            Revoked::AlreadyGone
        );
        // A body cut short by `MAX_ERROR_TEXT` still names invalid_token.
        let long = format!(
            r#"{{"error": "invalid_token", "error_description": "{}"}}"#,
            "x".repeat(4096)
        );
        let mock = MockSeq::new(vec![("/revoke", 400, long)]);
        assert_eq!(
            revoke(&guarded(&mock), &s(REFRESH), &s(CLIENT_SECRET)).unwrap(),
            Revoked::AlreadyGone
        );
    }

    #[test]
    fn a_refused_revoke_is_an_error_that_leaves_no_trace() {
        let stored = Secret::join(&[&s(CLIENT_ID), &s(CLIENT_SECRET), &s(REFRESH)]);
        let echo = format!(
            r#"{{"error":"invalid_request","error_description":"token {REFRESH} secret {CLIENT_SECRET}"}}"#
        );
        for (status, body) in [(400, echo.clone()), (500, echo)] {
            let mock = MockSeq::new(vec![("/revoke", status, body)]);
            let e = revoke_stored(&guarded(&mock), &stored).unwrap_err();
            assert_eq!(e.cause, Cause::Status(status));
            assert_no_key(&e, REFRESH);
            assert_no_key(&e, CLIENT_SECRET);
            assert_eq!(
                mock.sent()[0].body.as_deref().unwrap(),
                format!("token={REFRESH}")
            );
        }
        // A stored value that is not a sign-in is refused before any request.
        let mock = MockSeq::new(vec![]);
        assert!(revoke_stored(&guarded(&mock), &s("fk-only-one-part")).is_err());
        assert!(mock.sent().is_empty());
    }

    #[test]
    fn a_sign_in_refused_after_tokens_were_issued_revokes_nothing() {
        let narrow = include_str!("../fixtures/gemini/token_exchange.json")
            .replace(SCOPE, "https://www.googleapis.com/auth/userinfo.email");
        let mock = MockSeq::new(vec![("/token", 200, narrow)]);
        let e = exchange(
            &guarded(&mock),
            &s(CLIENT_ID),
            &s(CLIENT_SECRET),
            &s(CODE),
            &s(VERIFIER),
            "http://127.0.0.1:4242",
        )
        .err()
        .expect("a grant without the scope was accepted");
        // Revoking would withdraw the consent every other install of this client shares.
        assert_eq!(
            mock.sent().len(),
            1,
            "a refused sign-in called Google again"
        );
        assert!(e.message.as_str().contains(REVOKE_HINT), "{e:?}");
        for key in [CODE, VERIFIER, CLIENT_SECRET] {
            assert_no_key(&e, key);
        }
    }

    #[test]
    fn export_tables_follow_the_page_token() {
        let first = include_str!("../fixtures/gemini/tables.json")
            .replace(
                r#""totalItems":2"#,
                r#""totalItems":3,"nextPageToken":"fk-page-2""#,
            )
            .replace("gcp_billing_export_v1_000000_AAAAAA_BBBBBB", "notes_a");
        let second = include_str!("../fixtures/gemini/tables.json");
        let mock = MockSeq::new(vec![
            ("/tables?maxResults=1000", 200, first),
            ("&pageToken=fk-page-2", 200, second.into()),
        ]);
        let tables = export_tables(
            &guarded(&mock),
            &s("fk-access-0123456789"),
            &s("fk-scrub-0123456789"),
            "example-studio-1234",
            "billing_export",
        )
        .unwrap();
        assert_eq!(tables, ["gcp_billing_export_v1_000000_AAAAAA_BBBBBB"]);
        assert!(mock.unused().is_empty());
    }

    #[test]
    fn discovery_reads_the_location_and_the_export_tables() {
        let mock = MockSeq::new(vec![
            (
                "/datasets/billing_export",
                200,
                include_str!("../fixtures/gemini/dataset.json").into(),
            ),
            (
                "/tables?maxResults=1000",
                200,
                include_str!("../fixtures/gemini/tables.json").into(),
            ),
        ]);
        let access = s("fk-access-0123456789");
        let scrub = s("fk-scrub-0123456789");
        let http = guarded(&mock);
        assert_eq!(
            dataset_location(
                &http,
                &access,
                &scrub,
                "example-studio-1234",
                "billing_export"
            )
            .unwrap(),
            "US"
        );
        assert_eq!(
            export_tables(
                &http,
                &access,
                &scrub,
                "example-studio-1234",
                "billing_export"
            )
            .unwrap(),
            ["gcp_billing_export_v1_000000_AAAAAA_BBBBBB"]
        );
        assert_eq!(
            mock.sent()[0].header("Authorization"),
            Some("Bearer fk-access-0123456789")
        );
    }

    #[test]
    fn a_discovery_error_echoing_the_bare_access_token_leaves_no_trace() {
        let access = s("fk-access-BARE0123456789");
        let mock = MockSeq::new(vec![(
            "/datasets/billing_export",
            400,
            r#"{"error":{"message":"bad token fk-access-BARE0123456789"}}"#.into(),
        )]);
        let e = dataset_location(
            &guarded(&mock),
            &access,
            &s("fk-scrub-0123456789"),
            "example-studio-1234",
            "billing_export",
        )
        .unwrap_err();
        assert_no_key(&e, "fk-access-BARE0123456789");
    }

    #[test]
    fn loopback_cuts_off_a_peer_trickling_bytes() {
        // One byte every 50 ms never finishes a line; the 300 ms cap must end it, then the code lands.
        let (l, port) = loopback(5000, 300);
        let trickle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            while started.elapsed() < Duration::from_millis(1500) {
                if s.write_all(b"G").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            started.elapsed()
        });
        let code = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            send(
                port,
                &get(&format!("state={STATE}&code=fk-code-AFTERTRICKLE")),
            )
        });
        let started = Instant::now();
        let got = l.wait_for_code(STATE).unwrap();
        assert_eq!(got.expose(), "fk-code-AFTERTRICKLE");
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "the trickling peer held the listener for {:?}",
            started.elapsed()
        );
        let _ = trickle.join();
        assert!(code.join().unwrap().contains("signed in"));
    }
}
