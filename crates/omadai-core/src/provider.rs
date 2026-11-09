//! The provider interface and the registry `fetch` and `key add` go through.

use crate::cache::{BalanceHistory, MonthCache};
use crate::config::Config;
use crate::date::{Date, Month};
use crate::http::Transport;
use crate::scrub::{Scrubbed, scrub};
use crate::secret::Secret;

/// The one-time result `key add` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The credential can read spend; the text says what was read.
    Ok(String),
    /// The credential works but cannot read spend; the text names what is needed.
    Requirement(String),
    Failed(Scrubbed),
    /// No provider is built for this platform yet.
    NoProvider,
}

/// What kind of failure a `ProviderError` is, so the error log can say it in omadai's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Status(u16),
    Redirect(u16),
    Transport,
    Refused,
    MissingCredential,
    /// Google refused the stored refresh token (`invalid_grant`), so the person must sign in again.
    SignInExpired,
    /// Google refused the refresh under a Workspace's Google Cloud session control (`invalid_rapt`).
    SessionPolicy,
    /// Google no longer knows the stored OAuth client (`invalid_client`): it was deleted or its secret rotated.
    ClientGone,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub message: Scrubbed,
    /// Set when the failure is a missing permission or account type rather than a fault.
    pub requirement: Option<String>,
    pub cause: Cause,
    /// Set when the month is before the platform's history, which starts on this day; not a fault.
    pub before_history: Option<Date>,
}

/// A labelled credential handed to a provider for one fetch.
pub struct Credential {
    pub label: String,
    /// The config's series ID, which keys this credential's balance history (Flux, Fish Audio).
    pub series: String,
    pub secret: Secret,
}

pub trait Provider: Send + Sync {
    fn platform(&self) -> &'static str;
    fn check(&self, http: &dyn Transport, credential: &Secret, config: &Config) -> CheckOutcome;
    fn fetch_month(
        &self,
        http: &dyn Transport,
        credentials: &[Credential],
        month: Month,
        config: &Config,
    ) -> Result<MonthCache, ProviderError>;

    /// Whether spend is derived from balance readings, so `fetch::run` calls `fetch_derived`.
    fn derived(&self) -> bool {
        false
    }

    /// Appends this fetch's readings to `history` and derives the month from it; `fetch::run`
    /// saves `history` only when this succeeds.
    fn fetch_derived(
        &self,
        http: &dyn Transport,
        credentials: &[Credential],
        month: Month,
        config: &Config,
        history: &mut BalanceHistory,
    ) -> Result<MonthCache, ProviderError> {
        let _ = history;
        self.fetch_month(http, credentials, month, config)
    }
}

#[derive(Default)]
pub struct Registry {
    providers: Vec<Box<dyn Provider>>,
}

impl Registry {
    /// The providers shipped in this build: every v1 platform.
    pub fn builtin() -> Self {
        Registry::with(crate::providers::builtin())
    }

    pub fn with(providers: Vec<Box<dyn Provider>>) -> Self {
        Registry { providers }
    }

    pub fn get(&self, platform: &str) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|p| p.platform() == platform)
            .map(|p| p.as_ref())
    }

    /// The `key add` hook; every text it returns is scrubbed again with the credential checked.
    pub fn check(
        &self,
        platform: &str,
        http: &dyn Transport,
        credential: &Secret,
        config: &Config,
    ) -> CheckOutcome {
        let again = |text: &str| scrub(text, &[credential]);
        match self.get(platform) {
            Some(p) => match p.check(http, credential, config) {
                CheckOutcome::Ok(what) => CheckOutcome::Ok(again(&what).as_str().to_string()),
                CheckOutcome::Requirement(what) => {
                    CheckOutcome::Requirement(again(&what).as_str().to_string())
                }
                CheckOutcome::Failed(e) => CheckOutcome::Failed(again(e.as_str())),
                CheckOutcome::NoProvider => CheckOutcome::NoProvider,
            },
            None => CheckOutcome::NoProvider,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpError, Request, Response};
    use crate::scrub::{REDACTED, plain};

    struct NoNet;
    impl Transport for NoNet {
        fn send(&self, _: &Request, _: &Secret) -> Result<Response, HttpError> {
            Err(HttpError::Refused("test".into()))
        }
    }

    /// Answers `check` with text quoting the key unscrubbed, in the outcome its number picks.
    struct Leaky(u8);

    impl Provider for Leaky {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, credential: &Secret, _: &Config) -> CheckOutcome {
            let text = format!("key {} seen", credential.expose());
            match self.0 {
                0 => CheckOutcome::Ok(text),
                1 => CheckOutcome::Requirement(text),
                _ => CheckOutcome::Failed(plain(text)),
            }
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            _: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            unreachable!()
        }
    }

    #[test]
    fn check_text_quoting_the_key_is_scrubbed() {
        let key = Secret::new("fk-check-LEAKEDKEYTAIL".into());
        for kind in 0..3 {
            let out = Registry::with(vec![Box::new(Leaky(kind))]).check(
                "openai",
                &NoNet,
                &key,
                &Config::default(),
            );
            let text = match &out {
                CheckOutcome::Ok(t) | CheckOutcome::Requirement(t) => t.clone(),
                CheckOutcome::Failed(t) => t.to_string(),
                CheckOutcome::NoProvider => panic!(),
            };
            assert_eq!(text, format!("key {REDACTED} seen"), "{out:?}");
        }
    }
}
