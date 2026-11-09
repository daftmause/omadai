//! One fetch run: take the lock, read credentials, call providers in parallel, write the cache.

use crate::cache::{BalanceHistory, Breakdown, CACHE_VERSION, FigureKind, MonthCache, Store};
use crate::config::Config;
use crate::date::{Month, now_rfc3339};
use crate::errlog::{self, Failure, Record};
use crate::error::Error;
use crate::http::Transport;
use crate::keystore::{Keystore, KeystoreError};
use crate::lock;
use crate::paths::Paths;
use crate::platform;
use crate::provider::{Credential, ProviderError, Registry};
use crate::scrub::{KeyCheck, Scrubber, plain, scrub};
use crate::secret::Secret;

#[derive(Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Another fetch holds the lock; nothing was done.
    AlreadyRunning,
    Ran(Summary),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub fetched: Vec<String>,
    pub failed: Vec<(String, String)>,
    /// Platforms whose fetch needs something from the person rather than failing.
    pub needs: Vec<(String, String)>,
    pub no_provider: Vec<String>,
    /// The error log could not be written or was moved aside; never a failed fetch.
    pub log_problem: Option<String>,
}

pub struct FetchRequest<'a> {
    pub paths: &'a Paths,
    pub config: &'a Config,
    pub keystore: &'a dyn Keystore,
    pub registry: &'a Registry,
    pub transport: &'a dyn Transport,
    pub platform: Option<&'a str>,
    pub month: Month,
}

/// Runs one fetch and records its failures in the error log, taking `log.lock` only under `lock`.
pub fn run(req: &FetchRequest<'_>) -> Result<FetchOutcome, Error> {
    let Some(_lock) =
        lock::try_acquire(&req.paths.lock_file()).map_err(|e| log_failure(req.paths, e))?
    else {
        return Ok(FetchOutcome::AlreadyRunning);
    };
    // One clock reading stamps both the cache and the log, so a failure's two times agree.
    let now = now_rfc3339();
    match run_locked(req, &now) {
        Ok((mut summary, records)) => {
            summary.log_problem = match errlog::record(req.paths, records, &now) {
                Ok(moved) => moved,
                Err(e) => Some(format!("could not write the error log: {e}")),
            };
            Ok(FetchOutcome::Ran(summary))
        }
        Err(e) => {
            // The run's error is what the caller reports, so a log write failure here has nowhere to go.
            let _ = errlog::record(req.paths, vec![errlog::run_record(&e)], &now);
            Err(e)
        }
    }
}

/// Records a fetch that failed before it could run, such as an unreadable config, and hands it back.
pub fn log_failure(paths: &Paths, e: Error) -> Error {
    // The fetch's own error is what the caller reports, so a log write failure here has nowhere to go.
    let _ = errlog::record(paths, vec![errlog::run_record(&e)], &now_rfc3339());
    e
}

fn name_of(platform: &str) -> &'static str {
    platform::find(platform).map_or("the platform", |p| p.name)
}

/// The log record for a platform's failure; `detail` is the scrubbed message with any local problem.
fn failure_record(platform: &str, month: Month, e: &ProviderError, detail: &str) -> Record {
    Record::new(
        Some(platform),
        Some(month),
        errlog::summary(&Failure::Provider(e), Some(name_of(platform))),
        // Built only from a scrubbed message and omadai's own cache errors.
        plain(detail),
    )
}

/// Scrubs a provider's error again with every credential it was given, so no message a provider built can carry one out.
fn rescrub(e: ProviderError, creds: &[Credential]) -> ProviderError {
    let secrets: Vec<&Secret> = creds.iter().map(|c| &c.secret).collect();
    ProviderError {
        message: scrub(e.message.as_str(), &secrets),
        requirement: e
            .requirement
            .map(|r| scrub(&r, &secrets).as_str().to_string()),
        ..e
    }
}

/// Which kind of string a fresh month or the history beside it holds key material in (`an id`, `a name`, `a series` or `a label`), so nothing is saved for the platform this run.
pub(crate) fn key_in_ids(
    m: &MonthCache,
    h: Option<&BalanceHistory>,
    check: &KeyCheck,
) -> Option<&'static str> {
    let has = |s: &String| check.holds(s);
    if m.daily
        .values()
        .flat_map(|by_id| by_id.keys())
        .chain(m.names.keys())
        .any(has)
    {
        return Some("an id");
    }
    if m.names.values().any(has) {
        return Some("a name");
    }
    let h = h?;
    if h.points
        .iter()
        .map(|p| &p.series)
        .chain(h.labels.keys())
        .any(has)
    {
        return Some("a series");
    }
    h.labels.values().any(has).then_some("a label")
}

/// The id cached spend is moved to when its own id held key material, so the month total stays exact.
pub const WITHHELD_ID: &str = "(withheld)";

/// The name shown for [`WITHHELD_ID`] and for a withheld balance series or label.
pub const WITHHELD_NAME: &str = "id withheld: it held key material";

/// A loaded balance history with each series holding key material renamed `(withheld) <n>` and each such label replaced, so its readings keep their spend; a key rotated away since is not recognised.
fn without_keys(mut h: BalanceHistory, check: &KeyCheck) -> BalanceHistory {
    // New withheld series are numbered after any saved earlier, so two never share a name.
    let taken = h
        .points
        .iter()
        .map(|p| p.series.as_str())
        .chain(h.labels.keys().map(String::as_str))
        .filter_map(|s| s.strip_prefix(WITHHELD_ID)?.trim().parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let mut renamed: Vec<String> = Vec::new();
    let mut rename = |series: &str| -> String {
        let at = match renamed.iter().position(|s| s == series) {
            Some(at) => at,
            None => {
                renamed.push(series.to_string());
                renamed.len() - 1
            }
        };
        format!("{WITHHELD_ID} {}", taken + at + 1)
    };
    for p in &mut h.points {
        if check.holds(&p.series) {
            p.series = rename(&p.series);
        }
    }
    h.labels = std::mem::take(&mut h.labels)
        .into_iter()
        .map(|(series, label)| {
            let series = if check.holds(&series) {
                rename(&series)
            } else {
                series
            };
            let label = if check.holds(&label) {
                WITHHELD_NAME.to_string()
            } else {
                label
            };
            (series, label)
        })
        .collect();
    for series in h
        .points
        .iter()
        .map(|p| &p.series)
        .filter(|s| s.starts_with(WITHHELD_ID))
    {
        h.labels
            .entry(series.clone())
            .or_insert_with(|| WITHHELD_NAME.to_string());
    }
    h
}

/// A fresh month's warnings and `data_as_of` scrubbed with the platform's credentials, just before it is saved.
fn scrub_notes(mut m: MonthCache, scrubber: &Scrubber) -> MonthCache {
    for w in m.warnings.iter_mut().chain(m.credits_warnings.iter_mut()) {
        *w = scrubber.scrub(w.as_str());
    }
    m.data_as_of = m
        .data_as_of
        .map(|d| scrubber.scrub(&d).as_str().to_string());
    m
}

fn run_locked(req: &FetchRequest<'_>, now: &str) -> Result<(Summary, Vec<Record>), Error> {
    let mut records = Vec::new();
    // A series `Config::load` minted is saved before anything is recorded under it, so every later
    // load sees the same one.
    let persisted;
    let config = if req.config.keys.iter().any(|k| k.minted) {
        let _keys = lock::acquire(&req.paths.keys_lock_file())?;
        persisted = req.config.persist_minted(&req.paths.config_file())?;
        &persisted
    } else {
        req.config
    };
    let mut platforms: Vec<&str> = Vec::new();
    for k in &config.keys {
        if req.platform.is_none_or(|p| p == k.platform) && !platforms.contains(&k.platform.as_str())
        {
            platforms.push(&k.platform);
        }
    }
    let mut summary = Summary::default();
    if platforms.is_empty() {
        return Ok((summary, records));
    }
    // Fail before any provider runs; a locked keyring is never unlocked from here.
    if req.keystore.is_locked()? {
        return Err(KeystoreError::Locked.into());
    }
    let mut jobs = Vec::new();
    for platform in platforms {
        let Some(provider) = req.registry.get(platform) else {
            summary.no_provider.push(platform.to_string());
            continue;
        };
        let mut credentials = Vec::new();
        let mut missing = Vec::new();
        for k in config.entries(platform) {
            match req.keystore.get(platform, &k.label)? {
                Some(secret) => credentials.push(Credential {
                    label: k.label.clone(),
                    series: k.series.clone(),
                    secret,
                }),
                None => missing.push(format!("{:?}", k.label)),
            }
        }
        // A partial set would overwrite the platform's month with only some credentials' figures,
        // so the provider is not run.
        let refused = (!missing.is_empty()).then(|| {
            crate::providers::missing_credential(format!(
                "a key label is configured but no credential is stored for {}; run omadai key add {platform} --label <label>, or omadai key remove {platform} <label>",
                missing.join(", ")
            ))
        });
        jobs.push((platform, provider, credentials, refused));
    }
    let store = Store::new(&req.paths.state_dir);
    type Outcome = (Result<MonthCache, ProviderError>, Option<BalanceHistory>);
    let results: Vec<(&str, Outcome)> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(platform, provider, creds, refused)| {
                let store = &store;
                (
                    *platform,
                    s.spawn(move || -> Outcome {
                        if let Some(e) = refused {
                            return (Err(e.clone()), None);
                        }
                        match creds {
                            creds if provider.derived() => {
                                let secrets: Vec<&Secret> =
                                    creds.iter().map(|c| &c.secret).collect();
                                let mut history = match store.load_balance(platform) {
                                    Ok(Some(h)) => without_keys(h, &KeyCheck::new(&secrets)),
                                    Ok(None) => BalanceHistory {
                                        version: CACHE_VERSION,
                                        platform: platform.to_string(),
                                        points: Vec::new(),
                                        labels: Default::default(),
                                    },
                                    Err(e) => {
                                        return (
                                            Err(crate::providers::fail(format!(
                                                "could not read the balance history: {e}"
                                            ))),
                                            None,
                                        );
                                    }
                                };
                                let r = provider
                                    .fetch_derived(
                                        req.transport,
                                        creds,
                                        req.month,
                                        config,
                                        &mut history,
                                    )
                                    .map_err(|e| rescrub(e, creds));
                                (r, Some(history))
                            }
                            creds => (
                                provider
                                    .fetch_month(req.transport, creds, req.month, config)
                                    .map_err(|e| rescrub(e, creds)),
                                None,
                            ),
                        }
                    }),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(p, h)| {
                let r = h
                    .join()
                    .unwrap_or_else(|_| (Err(crate::providers::fail("provider crashed")), None));
                (p, r)
            })
            .collect()
    });
    // Each platform's needles are built now so the credentials are dropped before the save loop.
    let checks: Vec<(&str, KeyCheck, Scrubber)> = jobs
        .iter()
        .map(|(p, _, creds, _)| {
            let secrets: Vec<&Secret> = creds.iter().map(|c| &c.secret).collect();
            (*p, KeyCheck::new(&secrets), Scrubber::new(&secrets))
        })
        .collect();
    drop(jobs);
    let now = now.to_string();
    // `results` and `checks` both follow the order of `jobs`.
    for ((platform, (result, history)), (_, check, scrubber)) in results.into_iter().zip(&checks) {
        let held_in = match &result {
            Ok(fresh) => key_in_ids(fresh, history.as_ref(), check),
            Err(_) => None,
        };
        // A month derived from readings is saved only beside the history that holds them, and
        // neither is saved when a provider put key material in an id, name, series or label.
        let result = match (result, history) {
            (Ok(_), _) if held_in.is_some() => Err(crate::providers::fail(format!(
                "{} returned key material in {}; nothing was saved",
                name_of(platform),
                held_in.unwrap_or("an id")
            ))),
            (Ok(fresh), Some(h)) => match store.save_balance(&h) {
                Ok(()) => Ok(fresh),
                Err(e) => Err(crate::providers::fail(format!(
                    "could not save the balance history: {e}"
                ))),
            },
            (r, _) => r,
        };
        match result {
            Ok(fresh) => {
                let mut fresh = scrub_notes(fresh, scrubber);
                if let Some(from) = fresh.history_from {
                    // Days before the rolling floor can no longer be fetched, so cached ones are kept;
                    // an unreadable cache is left as it is rather than replaced by the shorter month.
                    match store.load_month(platform, req.month) {
                        Ok(Some(old)) if old.last_fetched.is_some() => {
                            for (day, by_id) in old.daily.range(..from) {
                                for (id, amount) in by_id {
                                    // A carried id holding key material moves to the withheld id, so saving replaces the poisoned file and the total stays exact.
                                    if check.holds(id) || id == WITHHELD_ID {
                                        *fresh
                                            .daily
                                            .entry(*day)
                                            .or_default()
                                            .entry(WITHHELD_ID.to_string())
                                            .or_default() += *amount;
                                        continue;
                                    }
                                    // Never added to a figure the fresh result already holds.
                                    let held = fresh.daily.entry(*day).or_default();
                                    if !held.contains_key(id) {
                                        held.insert(id.clone(), *amount);
                                    }
                                    if let Some(name) =
                                        old.names.get(id).filter(|n| !check.holds(n))
                                    {
                                        fresh
                                            .names
                                            .entry(id.clone())
                                            .or_insert_with(|| name.clone());
                                    }
                                }
                            }
                            fresh.daily.retain(|_, by_id| !by_id.is_empty());
                            let days = fresh
                                .daily
                                .values()
                                .filter(|by_id| by_id.contains_key(WITHHELD_ID))
                                .count();
                            if days > 0 {
                                fresh
                                    .names
                                    .insert(WITHHELD_ID.to_string(), WITHHELD_NAME.to_string());
                            }
                            // The day of the old fetch was still running, so the floor clears only when that fetch came on or after it.
                            let covered = old
                                .last_fetched
                                .as_deref()
                                .and_then(|t| t.get(..10))
                                .and_then(crate::date::Date::parse)
                                .is_some_and(|d| d >= from);
                            // Together the two fetches reach back to where the old one started.
                            if covered {
                                fresh.history_from = old.history_from.map(|h| h.min(from));
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let message = format!("could not read the cache: {err}");
                            let cause = crate::providers::fail(message.clone());
                            records.push(failure_record(platform, req.month, &cause, &message));
                            summary.failed.push((platform.to_string(), message));
                            continue;
                        }
                    }
                }
                fresh.last_fetched = Some(now.clone());
                fresh.last_error = None;
                fresh.last_error_at = None;
                fresh.requirement = None;
                match store.save_month(&fresh) {
                    Ok(()) => {
                        summary.fetched.push(platform.to_string());
                        if !fresh.warnings.is_empty() {
                            // One entry per fetch, so coalescing counts repeats rather than warnings.
                            let joined: Vec<&str> =
                                fresh.warnings.iter().map(|w| w.as_str()).collect();
                            records.push(Record::new(
                                Some(platform),
                                Some(req.month),
                                format!("Part of {}'s spend is missing", name_of(platform)),
                                // Each part was scrubbed by `scrub_notes`.
                                plain(joined.join("\n")),
                            ));
                        }
                        if !fresh.credits_warnings.is_empty() {
                            let joined: Vec<&str> =
                                fresh.credits_warnings.iter().map(|w| w.as_str()).collect();
                            records.push(Record::new(
                                Some(platform),
                                Some(req.month),
                                format!(
                                    "{}'s credits left could not be computed",
                                    name_of(platform)
                                ),
                                // Each part was scrubbed by `scrub_notes`.
                                plain(joined.join("\n")),
                            ));
                        }
                    }
                    Err(e) => {
                        let message = format!("could not save the cache: {e}");
                        let cause = crate::providers::fail(message.clone());
                        records.push(failure_record(platform, req.month, &cause, &message));
                        summary.failed.push((platform.to_string(), message));
                    }
                }
            }
            Err(e) if e.before_history.is_some() => {
                // Figures fetched while the month was in range stay, unmarked; otherwise the month
                // is cached as before history, so the next fetch is the provider's local check.
                let saved = match store.load_month(platform, req.month) {
                    // A placeholder is told apart by its floor, so a fetched month with no spend stays.
                    Ok(Some(mut old))
                        if old.last_fetched.is_some()
                            && !old.history_from.is_some_and(|h| h > req.month.last_day()) =>
                    {
                        old.last_fetched = Some(now.clone());
                        old.last_error = None;
                        old.last_error_at = None;
                        old.requirement = None;
                        store
                            .save_month(&old)
                            .map_err(|err| format!("could not save the cache: {err}"))
                    }
                    Ok(_) => {
                        let mut placeholder = MonthCache::new(
                            platform,
                            req.month,
                            FigureKind::Estimated,
                            Breakdown::Key,
                        );
                        placeholder.history_from = e.before_history;
                        placeholder.last_fetched = Some(now.clone());
                        store
                            .save_month(&placeholder)
                            .map_err(|err| format!("could not save the cache: {err}"))
                    }
                    Err(err) => Err(format!("could not read the cache: {err}")),
                };
                match saved {
                    Ok(()) => summary.fetched.push(platform.to_string()),
                    Err(message) => {
                        let cause = crate::providers::fail(message.clone());
                        records.push(failure_record(platform, req.month, &cause, &message));
                        summary.failed.push((platform.to_string(), message));
                    }
                }
            }
            Err(e) => {
                // Keep the last good figures and record the (already scrubbed) error or requirement
                // beside them, never both.
                // A platform's first fetch gets an empty placeholder with no `last_fetched`, so the row
                // still says what it needs or what failed.
                // An unreadable cache is left as it is, so the failure is only in the summary.
                let mut problems = Vec::new();
                let old = match store.load_month(platform, req.month) {
                    Ok(old) => Some(old.unwrap_or_else(|| {
                        MonthCache::new(platform, req.month, FigureKind::Estimated, Breakdown::Key)
                    })),
                    Err(err) => {
                        problems.push(format!("could not read the cache: {err}"));
                        None
                    }
                };
                if let Some(mut old) = old {
                    match &e.requirement {
                        Some(text) => {
                            old.requirement = Some(plain(text.clone()));
                            old.last_error = None;
                            old.last_error_at = Some(now.clone());
                        }
                        None => {
                            old.requirement = None;
                            old.last_error = Some(e.message.clone());
                            old.last_error_at = Some(now.clone());
                        }
                    }
                    if let Err(err) = store.save_month(&old) {
                        problems.push(format!("could not save the cache: {err}"));
                    }
                }
                let message = std::iter::once(e.message.to_string())
                    .chain(problems.iter().cloned())
                    .collect::<Vec<_>>()
                    .join("; ");
                if e.requirement.is_some() && problems.is_empty() {
                    summary.needs.push((platform.to_string(), message));
                } else {
                    records.push(failure_record(platform, req.month, &e, &message));
                    summary.failed.push((platform.to_string(), message));
                }
            }
        }
    }
    Ok((summary, records))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Breakdown, FigureKind};
    use crate::config::KeyEntry;
    use crate::fsutil::testdir::TempDir;
    use crate::http::{HttpError, Request, Response};
    use crate::keystore::memory::MemoryKeystore;
    use crate::provider::{CheckOutcome, Provider};
    use rust_decimal::Decimal;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    struct NoNet;
    impl Transport for NoNet {
        fn send(&self, _: &Request, _: &Secret) -> Result<Response, HttpError> {
            Err(HttpError::Refused("test".into()))
        }
    }

    struct Fake {
        calls: Arc<AtomicUsize>,
        fail_with_key: bool,
        needs: bool,
        hold: Duration,
    }

    impl Provider for Fake {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::Ok("fake".into())
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            creds: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.hold);
            if self.needs {
                return Err(crate::providers::requirement(
                    "OpenAI needs an Admin key".into(),
                ));
            }
            if self.fail_with_key {
                let raw = format!("401 invalid key {}", creds[0].secret.expose());
                return Err(ProviderError {
                    message: scrub(&raw, &[&creds[0].secret]),
                    ..crate::providers::fail("")
                });
            }
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.add(month.first_day(), "key_1", Decimal::new(125, 2));
            Ok(c)
        }
    }

    struct Env {
        _t: TempDir,
        paths: Paths,
        config: Config,
        keystore: MemoryKeystore,
    }

    const KEY: &str = "fk-admin-FETCHTESTKEY987654321";

    fn env() -> Env {
        let t = TempDir::new("fetch");
        let paths = Paths {
            home: Some(t.path().join("home")),
            config_dir: t.path().join("config"),
            state_dir: t.path().join("state"),
            keyrings_dir: t.path().join("keyrings"),
        };
        let config = Config {
            keys: vec![KeyEntry::new("openai", "work")],
            ..Config::default()
        };
        let keystore = MemoryKeystore::new();
        keystore
            .put("openai", "work", &Secret::new(KEY.into()))
            .unwrap();
        Env {
            _t: t,
            paths,
            config,
            keystore,
        }
    }

    fn request<'a>(e: &'a Env, registry: &'a Registry) -> FetchRequest<'a> {
        FetchRequest {
            paths: &e.paths,
            config: &e.config,
            keystore: &e.keystore,
            registry,
            transport: &NoNet,
            platform: None,
            month: Month::parse("2026-11").unwrap(),
        }
    }

    #[test]
    fn ten_contenders_one_round() {
        let e = env();
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Registry::with(vec![Box::new(Fake {
            calls: calls.clone(),
            fail_with_key: false,
            needs: false,
            hold: Duration::from_millis(400),
        })]);
        let barrier = Barrier::new(10);
        let outcomes: Vec<(FetchOutcome, Duration)> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..10)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        let start = Instant::now();
                        let r = run(&request(&e, &registry)).unwrap();
                        (r, start.elapsed())
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let ran: Vec<_> = outcomes
            .iter()
            .filter(|(o, _)| matches!(o, FetchOutcome::Ran(_)))
            .collect();
        assert_eq!(ran.len(), 1, "exactly one fetch proceeds");
        // A contender's `run` returns straight after `try_acquire`, so this times only that call;
        // a blocking acquire would wait out most of the 400 ms hold.
        for (o, took) in &outcomes {
            if *o == FetchOutcome::AlreadyRunning {
                assert!(
                    *took < Duration::from_millis(200),
                    "a contender waited {took:?}"
                );
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one network round");
        let cached = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            cached
                .daily
                .values()
                .flat_map(|m| m.values())
                .copied()
                .sum::<Decimal>(),
            Decimal::new(125, 2)
        );
        assert!(cached.last_fetched.is_some());
    }

    #[test]
    fn locked_keyring_fails_within_a_second() {
        let e = env();
        e.keystore.set_locked(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Registry::with(vec![Box::new(Fake {
            calls: calls.clone(),
            fail_with_key: false,
            needs: false,
            hold: Duration::ZERO,
        })]);
        let start = Instant::now();
        let err = run(&request(&e, &registry)).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(err.to_string().starts_with("keyring locked"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failure_keeps_last_good_and_stores_scrubbed_error() {
        let e = env();
        let ok = Registry::with(vec![Box::new(Fake {
            calls: Arc::default(),
            fail_with_key: false,
            needs: false,
            hold: Duration::ZERO,
        })]);
        run(&request(&e, &ok)).unwrap();
        let bad = Registry::with(vec![Box::new(Fake {
            calls: Arc::default(),
            fail_with_key: true,
            needs: false,
            hold: Duration::ZERO,
        })]);
        let FetchOutcome::Ran(summary) = run(&request(&e, &bad)).unwrap() else {
            panic!()
        };
        assert_eq!(summary.failed.len(), 1);
        assert!(!summary.failed[0].1.contains(KEY));
        let file =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        assert!(!file.contains(KEY) && !file.contains("fk-admin"), "{file}");
        assert!(file.contains("invalid key [redacted]"), "{file}");
        assert!(file.contains("\"1.25\""), "last good figure kept: {file}");
    }

    /// Builds its error from the key with no scrub, as a careless `fail` site would.
    struct Leaky {
        requirement: bool,
    }

    impl Provider for Leaky {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            creds: &[Credential],
            _: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            let text = format!("unexpected value {:?}", creds[0].secret.expose());
            Err(if self.requirement {
                crate::providers::requirement(text)
            } else {
                crate::providers::fail(text)
            })
        }
    }

    #[test]
    fn a_provider_message_quoting_the_key_is_scrubbed_at_the_boundary() {
        for requirement in [false, true] {
            let e = env();
            let registry = Registry::with(vec![Box::new(Leaky { requirement })]);
            let FetchOutcome::Ran(summary) = run(&request(&e, &registry)).unwrap() else {
                panic!()
            };
            let shown: Vec<&String> = summary
                .failed
                .iter()
                .chain(&summary.needs)
                .map(|(_, m)| m)
                .collect();
            assert_eq!(shown.len(), 1, "{summary:?}");
            assert!(
                !shown[0].contains("fk-admin") && shown[0].contains(crate::scrub::REDACTED),
                "{}",
                shown[0]
            );
            let cache =
                std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json"))
                    .unwrap();
            assert!(
                !cache.contains("fk-admin") && cache.contains(crate::scrub::REDACTED),
                "{cache}"
            );
            let log = std::fs::read_to_string(e.paths.errors_file()).unwrap_or_default();
            assert!(!log.contains("fk-admin"), "{log}");
            assert_eq!(log.contains(crate::scrub::REDACTED), !requirement, "{log}");
        }
    }

    /// Returns spend whose warning and `data_as_of` quote the key, as a careless provider would.
    struct LeakyOk;

    impl Provider for LeakyOk {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            creds: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            let key = creds[0].secret.expose();
            let mut m = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            m.add(month.first_day(), "key_a", Decimal::new(125, 2));
            m.names.insert("key_a".into(), "Key A".into());
            m.warnings
                .push(plain(format!("skipped a series for {key}")));
            m.data_as_of = Some(format!("as of {key}"));
            Ok(m)
        }
    }

    #[test]
    fn a_successful_fetch_quoting_the_key_in_a_warning_is_scrubbed_at_the_boundary() {
        let e = env();
        let registry = Registry::with(vec![Box::new(LeakyOk)]);
        let FetchOutcome::Ran(summary) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert_eq!(summary.fetched, vec!["openai".to_string()], "{summary:?}");
        let cache =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        assert!(
            !cache.contains("fk-admin") && cache.contains(crate::scrub::REDACTED),
            "{cache}"
        );
        let log = std::fs::read_to_string(e.paths.errors_file()).unwrap_or_default();
        assert!(
            !log.contains("fk-admin") && log.contains(crate::scrub::REDACTED),
            "{log}"
        );
    }

    /// Where `LeakyIds` puts the key.
    #[derive(Clone, Copy, Debug)]
    enum Leak {
        Id,
        Name,
        Series,
        Label,
    }

    /// Puts the key in one id, name, series or label, as a careless provider would.
    struct LeakyIds(Leak);

    impl Provider for LeakyIds {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            creds: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            let key = creds[0].secret.expose();
            let mut m = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            let id = match self.0 {
                Leak::Id => format!("model-{key}"),
                _ => "model".into(),
            };
            m.add(month.first_day(), &id, Decimal::new(900, 2));
            let name = match self.0 {
                Leak::Name => format!("named {key}"),
                _ => "Model".into(),
            };
            m.names.insert(id, name);
            Ok(m)
        }
        fn derived(&self) -> bool {
            matches!(self.0, Leak::Series | Leak::Label)
        }
        fn fetch_derived(
            &self,
            http: &dyn Transport,
            creds: &[Credential],
            month: Month,
            config: &Config,
            history: &mut BalanceHistory,
        ) -> Result<MonthCache, ProviderError> {
            let key = creds[0].secret.expose().to_string();
            let (series, label) = match self.0 {
                Leak::Series => (format!("wallet-{key}"), "Wallet".to_string()),
                _ => ("wallet".to_string(), format!("label {key}")),
            };
            history.points.push(crate::cache::BalancePoint {
                series: series.clone(),
                at: "2026-11-01T00:00:00Z".into(),
                balance: Decimal::new(500, 2),
                cumulative_top_up: None,
            });
            history.labels.insert(series, label);
            self.fetch_month(http, creds, month, config)
        }
    }

    #[test]
    fn a_key_in_an_id_name_or_series_fails_the_fetch_and_saves_nothing() {
        for leak in [Leak::Id, Leak::Name, Leak::Series, Leak::Label] {
            let e = env();
            fetch_with(&e, fake(false, false));
            let month_file = e.paths.state_dir.join("cache/openai/2026-11.json");
            let balance_file = e.paths.state_dir.join("balance/openai.json");
            let FetchOutcome::Ran(s) = run(&request(
                &e,
                &Registry::with(vec![Box::new(LeakyIds(leak))]),
            ))
            .unwrap() else {
                panic!()
            };
            assert_eq!(s.failed.len(), 1, "{leak:?}: {s:?}");
            assert!(s.failed[0].1.contains("key material"), "{leak:?}: {s:?}");
            let field = match leak {
                Leak::Id => "an id",
                Leak::Name => "a name",
                Leak::Series => "a series",
                Leak::Label => "a label",
            };
            assert!(s.failed[0].1.contains(field), "{leak:?}: {s:?}");
            assert!(!balance_file.exists(), "{leak:?}: no balance history saved");
            let cache = std::fs::read_to_string(&month_file).unwrap();
            assert!(!cache.contains("fk-admin"), "{leak:?}: {cache}");
            assert!(
                !cache.contains("\"9.00\""),
                "{leak:?}: nothing fresh saved: {cache}"
            );
            let r = report_of(&e);
            assert_eq!(
                r.platforms[0].total,
                Decimal::new(125, 2),
                "{leak:?}: the last good figure stays"
            );
            let json = crate::report::to_json(&r);
            assert!(!json.contains("fk-admin"), "{leak:?}: {json}");
        }
    }

    /// A derived provider whose failure quotes the key.
    struct FailingWallet;

    impl Provider for FailingWallet {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            _: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            Err(crate::providers::fail("derived only"))
        }
        fn derived(&self) -> bool {
            true
        }
        fn fetch_derived(
            &self,
            _: &dyn Transport,
            creds: &[Credential],
            _: Month,
            _: &Config,
            _: &mut BalanceHistory,
        ) -> Result<MonthCache, ProviderError> {
            Err(crate::providers::fail(format!(
                "bad wallet {}",
                creds[0].secret.expose()
            )))
        }
    }

    #[test]
    fn a_derived_failure_quoting_the_key_is_scrubbed_at_the_boundary() {
        let e = env();
        let FetchOutcome::Ran(s) =
            run(&request(&e, &Registry::with(vec![Box::new(FailingWallet)]))).unwrap()
        else {
            panic!()
        };
        assert_eq!(s.failed.len(), 1, "{s:?}");
        assert!(!s.failed[0].1.contains("fk-admin"), "{s:?}");
        let cache =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        assert!(!cache.contains("fk-admin"), "{cache}");
    }

    /// A Higgsfield-shaped `id:secret` key whose rows are keyed by its public id, or whose error quotes that id.
    struct PublicIds {
        fail: bool,
    }

    const PUBLIC: &str = "fkid123456";

    impl Provider for PublicIds {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            if self.fail {
                return Err(crate::providers::fail(format!("unknown key id {PUBLIC}")));
            }
            let mut m = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            m.add(month.first_day(), PUBLIC, Decimal::new(250, 2));
            m.names.insert(PUBLIC.into(), "work".into());
            Ok(m)
        }
    }

    fn with_public_key(e: &Env) {
        e.keystore.delete("openai", "work").unwrap();
        e.keystore
            .put(
                "openai",
                "work",
                &Secret::new(format!("{PUBLIC}:fk-secret-ABCDEFGHIJKLMNOP")),
            )
            .unwrap();
    }

    #[test]
    fn an_id_keyed_by_the_public_half_of_a_key_is_kept_and_an_error_quoting_it_is_redacted() {
        let e = env();
        with_public_key(&e);
        let ok = Registry::with(vec![Box::new(PublicIds { fail: false })]);
        let FetchOutcome::Ran(s) = run(&request(&e, &ok)).unwrap() else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        let cache = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(cache.names[PUBLIC], "work", "{cache:?}");
        let bad = Registry::with(vec![Box::new(PublicIds { fail: true })]);
        let FetchOutcome::Ran(s) = run(&request(&e, &bad)).unwrap() else {
            panic!()
        };
        assert_eq!(s.failed.len(), 1, "{s:?}");
        assert!(
            !s.failed[0].1.contains(PUBLIC) && s.failed[0].1.contains(crate::scrub::REDACTED),
            "{s:?}"
        );
    }

    /// A provider whose key name starts with the same eight characters as the key, as a vendor's prefix would.
    struct PrefixName;

    impl Provider for PrefixName {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            let mut m = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            m.add(month.first_day(), "key_1", Decimal::new(300, 2));
            m.names
                .insert("key_1".into(), "fk-admin dashboard key".into());
            Ok(m)
        }
    }

    #[test]
    fn a_name_sharing_only_the_keys_prefix_is_saved() {
        let e = env();
        let FetchOutcome::Ran(s) =
            run(&request(&e, &Registry::with(vec![Box::new(PrefixName)]))).unwrap()
        else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        let cache = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(cache.names["key_1"], "fk-admin dashboard key");
    }

    /// A rolling-window provider whose fresh result starts on the 5th and carries earlier days from the cache.
    struct Rolling;

    impl Provider for Rolling {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            let mut m = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            let from = crate::date::Date::parse("2026-11-05").unwrap();
            m.history_from = Some(from);
            m.add(from, "clean", Decimal::new(200, 2));
            m.names.insert("clean".into(), "Clean".into());
            Ok(m)
        }
    }

    #[test]
    fn a_carried_day_holding_the_key_moves_to_the_withheld_id_and_keeps_the_total() {
        let e = env();
        let store = Store::new(&e.paths.state_dir);
        let month = Month::parse("2026-11").unwrap();
        let day1 = crate::date::Date::parse("2026-11-01").unwrap();
        let mut old = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
        old.add(day1, &format!("model-{KEY}"), Decimal::new(400, 2));
        old.add(day1, "clean", Decimal::new(300, 2));
        old.add(day1, "named", Decimal::new(100, 2));
        old.names.insert("named".into(), format!("named {KEY}"));
        old.names.insert("clean".into(), "Clean".into());
        old.last_fetched = Some("2026-11-03T00:00:00Z".into());
        store.save_month(&old).unwrap();
        let FetchOutcome::Ran(s) =
            run(&request(&e, &Registry::with(vec![Box::new(Rolling)]))).unwrap()
        else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        let file =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        assert!(!file.contains("fk-admin"), "{file}");
        let r = report_of(&e);
        assert_eq!(r.platforms[0].total, Decimal::new(1000, 2), "{r:?}");
        let withheld = r.platforms[0]
            .items
            .iter()
            .find(|i| i.id == WITHHELD_ID)
            .expect("withheld row");
        assert_eq!(withheld.total, Decimal::new(400, 2));
        assert_eq!(withheld.name.as_deref(), Some(WITHHELD_NAME));
        // The row's name explains it, so no warning marks the month incomplete.
        assert!(r.platforms[0].warnings.is_empty(), "{r:?}");
        assert!(!crate::report::to_json(&r).contains("fk-admin"));
        // The next fetch carries the withheld row on, still named and still without a warning.
        run(&request(&e, &Registry::with(vec![Box::new(Rolling)]))).unwrap();
        let again = report_of(&e);
        assert_eq!(again.platforms[0].total, Decimal::new(1000, 2), "{again:?}");
        assert!(again.platforms[0].warnings.is_empty(), "{again:?}");
        assert!(
            again.platforms[0]
                .items
                .iter()
                .any(|i| i.id == WITHHELD_ID && i.name.as_deref() == Some(WITHHELD_NAME)),
            "{again:?}"
        );
    }

    /// A derived provider that appends one clean reading to whatever history it was given.
    struct CleanWallet;

    impl Provider for CleanWallet {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::NoProvider
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            _: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            Err(crate::providers::fail("derived only"))
        }
        fn derived(&self) -> bool {
            true
        }
        fn fetch_derived(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
            history: &mut BalanceHistory,
        ) -> Result<MonthCache, ProviderError> {
            history.points.push(crate::cache::BalancePoint {
                series: "wallet".into(),
                at: "2026-11-02T00:00:00Z".into(),
                balance: Decimal::new(500, 2),
                cumulative_top_up: None,
            });
            history.labels.insert("wallet".into(), "Wallet".into());
            let mut m = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            m.add(month.first_day(), "wallet", Decimal::new(100, 2));
            Ok(m)
        }
    }

    #[test]
    fn a_series_withheld_on_a_later_load_takes_a_new_number() {
        let point = |series: &str| crate::cache::BalancePoint {
            series: series.to_string(),
            at: "2026-11-01T00:00:00Z".into(),
            balance: Decimal::new(100, 2),
            cumulative_top_up: None,
        };
        let history = |points| BalanceHistory {
            version: CACHE_VERSION,
            platform: "flux".into(),
            points,
            labels: Default::default(),
        };
        let b = Secret::new("fk-key-bbbbbbbbbbbbbbbb".into());
        let a = Secret::new("fk-key-aaaaaaaaaaaaaaaa".into());
        let first = without_keys(
            history(vec![point("s-fk-key-bbbbbbbbbbbbbbbb")]),
            &KeyCheck::new(&[&b]),
        );
        assert_eq!(first.points[0].series, format!("{WITHHELD_ID} 1"));
        let mut points = first.points;
        points.push(point("s-fk-key-aaaaaaaaaaaaaaaa"));
        let second = without_keys(history(points), &KeyCheck::new(&[&a, &b]));
        let series: std::collections::BTreeSet<_> =
            second.points.iter().map(|p| p.series.clone()).collect();
        assert_eq!(series.len(), 2, "{series:?}");
        assert!(series.contains(&format!("{WITHHELD_ID} 2")), "{series:?}");
    }

    #[test]
    fn a_loaded_balance_series_holding_the_key_is_withheld_and_the_fetch_saves() {
        let e = env();
        let store = Store::new(&e.paths.state_dir);
        let mut labels = std::collections::BTreeMap::new();
        labels.insert(format!("wallet-{KEY}"), "Old".to_string());
        labels.insert("old".to_string(), format!("label {KEY}"));
        store
            .save_balance(&BalanceHistory {
                version: CACHE_VERSION,
                platform: "openai".into(),
                points: vec![crate::cache::BalancePoint {
                    series: format!("wallet-{KEY}"),
                    at: "2026-11-01T00:00:00Z".into(),
                    balance: Decimal::new(900, 2),
                    cumulative_top_up: None,
                }],
                labels: labels.into_iter().collect(),
            })
            .unwrap();
        let FetchOutcome::Ran(s) =
            run(&request(&e, &Registry::with(vec![Box::new(CleanWallet)]))).unwrap()
        else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        let balance =
            std::fs::read_to_string(e.paths.state_dir.join("balance/openai.json")).unwrap();
        assert!(!balance.contains("fk-admin"), "{balance}");
        assert!(balance.contains("\"wallet\""), "{balance}");
        assert!(balance.contains("(withheld) 1"), "{balance}");
        let r = report_of(&e);
        assert!(r.platforms[0].warnings.is_empty(), "{r:?}");
    }

    fn fetch_with(e: &Env, fake: Fake) {
        run(&request(e, &Registry::with(vec![Box::new(fake)]))).unwrap();
    }

    fn fake(fail_with_key: bool, needs: bool) -> Fake {
        Fake {
            calls: Arc::default(),
            fail_with_key,
            needs,
            hold: Duration::ZERO,
        }
    }

    fn report_of(e: &Env) -> crate::report::Report {
        let range = crate::report::Range::month(Month::parse("2026-11").unwrap());
        crate::report::build(&Store::new(&e.paths.state_dir), range, None).unwrap()
    }

    #[test]
    fn requirement_row_is_stored_and_reported_as_a_requirement_then_cleared() {
        let e = env();
        fetch_with(&e, fake(false, false));
        // A fault first, so the requirement must also displace it.
        fetch_with(&e, fake(true, false));
        assert!(report_of(&e).platforms[0].last_error.is_some());

        fetch_with(&e, fake(false, true));
        let r = report_of(&e);
        let p = &r.platforms[0];
        assert_eq!(
            p.requirement.as_ref().map(|n| n.as_str()),
            Some("OpenAI needs an Admin key")
        );
        assert!(p.last_error.is_none(), "a requirement is not an error");
        assert_eq!(p.total, Decimal::new(125, 2), "last good figure kept");
        let json: serde_json::Value = serde_json::from_str(&crate::report::to_json(&r)).unwrap();
        assert_eq!(
            json["platforms"][0]["requirement"],
            "OpenAI needs an Admin key"
        );
        assert!(json["platforms"][0]["last_error"].is_null());
        let text = crate::report::to_text(&r);
        assert!(
            text.contains("needs (2026-11): OpenAI needs an Admin key"),
            "{text}"
        );
        assert!(!text.contains("last error"), "{text}");

        fetch_with(&e, fake(false, false));
        let r = report_of(&e);
        assert!(r.platforms[0].requirement.is_none() && r.platforms[0].last_error.is_none());
        let file =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        assert!(!file.contains("requirement"), "{file}");
    }

    #[test]
    fn first_fetch_requirement_still_makes_a_row() {
        let e = env();
        fetch_with(&e, fake(false, true));
        let r = report_of(&e);
        let p = &r.platforms[0];
        assert_eq!(
            p.requirement.as_ref().map(|n| n.as_str()),
            Some("OpenAI needs an Admin key")
        );
        assert!(p.last_fetched.is_none());
        let text = crate::report::to_text(&r);
        assert!(text.contains("not fetched yet"), "{text}");
        assert!(
            text.contains("needs (2026-11): OpenAI needs an Admin key"),
            "{text}"
        );
    }

    #[test]
    fn missing_credential_fails_the_platform_and_keeps_the_last_good_cache() {
        let mut e = env();
        fetch_with(&e, fake(false, false));
        let before = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        e.config.keys.push(KeyEntry::new("openai", "gone"));
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Registry::with(vec![Box::new(Fake {
            calls: calls.clone(),
            fail_with_key: false,
            needs: false,
            hold: Duration::ZERO,
        })]);
        let FetchOutcome::Ran(s) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert!(s.fetched.is_empty(), "{s:?}");
        assert_eq!(s.failed.len(), 1, "{s:?}");
        assert!(s.failed[0].1.contains("\"gone\""), "{s:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no partial fetch");
        let after = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(after.daily, before.daily);
        assert_eq!(after.last_fetched, before.last_fetched);
        assert!(
            after
                .last_error
                .as_ref()
                .is_some_and(|m| m.as_str().contains("\"gone\"")),
            "{after:?}"
        );
    }

    /// Always succeeds for Anthropic, so a run can show one platform failing while another proceeds.
    struct AnthropicOk;

    impl Provider for AnthropicOk {
        fn platform(&self) -> &'static str {
            "anthropic"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::Ok("fake".into())
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            let mut c = MonthCache::new("anthropic", month, FigureKind::Billed, Breakdown::Key);
            c.add(month.first_day(), "apikey_1", Decimal::new(2, 0));
            Ok(c)
        }
    }

    fn with_anthropic(e: &mut Env) {
        e.config.keys.push(KeyEntry::new("anthropic", "org"));
        e.keystore
            .put("anthropic", "org", &Secret::new("fk-anthropic-1".into()))
            .unwrap();
    }

    #[test]
    fn unwritable_cache_fails_that_platform_and_the_run_continues() {
        let mut e = env();
        with_anthropic(&mut e);
        // A file where the platform's cache directory belongs makes every save for it fail.
        std::fs::create_dir_all(e.paths.state_dir.join("cache")).unwrap();
        std::fs::write(e.paths.state_dir.join("cache/openai"), "").unwrap();
        let registry = Registry::with(vec![Box::new(fake(false, false)), Box::new(AnthropicOk)]);
        let FetchOutcome::Ran(s) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["anthropic".to_string()], "{s:?}");
        assert_eq!(s.failed.len(), 1, "{s:?}");
        assert_eq!(s.failed[0].0, "openai");
        assert!(s.failed[0].1.contains("could not save the cache"), "{s:?}");
    }

    #[test]
    fn unreadable_cache_on_failure_is_that_platforms_failure() {
        let mut e = env();
        with_anthropic(&mut e);
        let dir = e.paths.state_dir.join("cache/openai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-11.json"), "not json").unwrap();
        let registry = Registry::with(vec![Box::new(fake(true, false)), Box::new(AnthropicOk)]);
        let FetchOutcome::Ran(s) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["anthropic".to_string()], "{s:?}");
        assert_eq!(s.failed[0].0, "openai");
        assert!(s.failed[0].1.contains("could not read the cache"), "{s:?}");
        assert_eq!(
            std::fs::read(dir.join("2026-11.json")).unwrap(),
            b"not json",
            "an unreadable cache is not overwritten"
        );
        let r = report_of(&e);
        let openai = r.platforms.iter().find(|p| p.platform == "openai").unwrap();
        assert!(
            openai.warnings[0].contains("unreadable cache"),
            "{:?}",
            openai.warnings
        );
    }

    #[test]
    fn requirement_is_listed_as_a_need_not_a_failure() {
        let e = env();
        let registry = Registry::with(vec![Box::new(fake(false, true))]);
        let FetchOutcome::Ran(s) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert!(s.failed.is_empty(), "{s:?}");
        assert_eq!(s.needs.len(), 1, "{s:?}");
        assert!(s.needs[0].1.contains("Admin key"), "{s:?}");
    }

    #[test]
    fn derived_platform_keeps_its_balance_history_across_runs() {
        use crate::http::Guarded;
        use crate::http::mock::MockSeq;
        let mut e = env();
        e.config.keys = vec![KeyEntry::new("flux", "render")];
        e.keystore
            .put("flux", "render", &Secret::new("fk-bfl-FETCHTEST000".into()))
            .unwrap();
        let registry = Registry::with(vec![Box::new(crate::providers::flux::Flux {
            now: || "2026-11-03T09:00:00Z".into(),
        })]);
        let store = Store::new(&e.paths.state_dir);
        let nov = Month::parse("2026-11").unwrap();
        let run_with = |status: u16, body: &str| {
            let mock = MockSeq::new(vec![("/v1/credits", status, body.into())]);
            let transport = Guarded::new(&mock);
            let mut r = request(&e, &registry);
            r.transport = &transport;
            let FetchOutcome::Ran(s) = run(&r).unwrap() else {
                panic!()
            };
            s
        };
        let s = run_with(200, r#"{"credits": 900}"#);
        assert_eq!(s.fetched, vec!["flux".to_string()], "{s:?}");
        assert_eq!(store.load_balance("flux").unwrap().unwrap().points.len(), 1);
        assert!(
            store
                .load_month("flux", nov)
                .unwrap()
                .unwrap()
                .daily
                .is_empty()
        );

        // A failed read appends nothing.
        let s = run_with(500, "{}");
        assert_eq!(s.failed.len(), 1, "{s:?}");
        assert_eq!(store.load_balance("flux").unwrap().unwrap().points.len(), 1);

        run_with(200, r#"{"credits": 650}"#);
        let h = store.load_balance("flux").unwrap().unwrap();
        assert_eq!(h.points.len(), 2);
        let month = store.load_month("flux", nov).unwrap().unwrap();
        assert_eq!(month.kind, FigureKind::Derived);
        assert_eq!(
            month.daily[&nov.first_day().succ().succ()][&e.config.keys[0].series],
            Decimal::new(25, 1)
        );
        assert_eq!(month.credits_remaining, Some(Decimal::new(65, 1)));
    }

    #[test]
    fn unreadable_balance_history_fails_the_platform_and_is_left_alone() {
        let mut e = env();
        e.config.keys = vec![KeyEntry::new("flux", "render")];
        e.keystore
            .put("flux", "render", &Secret::new("fk-bfl-FETCHTEST000".into()))
            .unwrap();
        let dir = e.paths.state_dir.join("balance");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("flux.json"), "not json").unwrap();
        let registry = Registry::with(vec![Box::new(crate::providers::flux::Flux::default())]);
        let FetchOutcome::Ran(s) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert_eq!(s.failed.len(), 1, "{s:?}");
        assert!(
            s.failed[0].1.contains("could not read the balance history"),
            "{s:?}"
        );
        assert_eq!(std::fs::read(dir.join("flux.json")).unwrap(), b"not json");
    }

    /// A configured entry as `key add` writes it.
    fn entry(platform: &str, label: &str) -> KeyEntry {
        KeyEntry::new(platform, label)
    }

    /// Runs one Flux fetch for November reading `credits`, with the clock at `at`.
    fn flux_run(e: &Env, at: &'static str, credits: &str) -> MonthCache {
        use crate::http::Guarded;
        use crate::http::mock::MockSeq;
        // The provider runs on a fetch thread, so the clock is a static and the runs take turns.
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        static CLOCK: std::sync::Mutex<&str> = std::sync::Mutex::new("");
        let _turn = TURN.lock().unwrap_or_else(|p| p.into_inner());
        *CLOCK.lock().unwrap() = at;
        let registry = Registry::with(vec![Box::new(crate::providers::flux::Flux {
            now: || CLOCK.lock().unwrap().to_string(),
        })]);
        let mock = MockSeq::new(vec![(
            "/v1/credits",
            200,
            format!(r#"{{"credits": {credits}}}"#),
        )]);
        let transport = Guarded::new(&mock);
        let mut r = request(e, &registry);
        r.transport = &transport;
        let FetchOutcome::Ran(s) = run(&r).unwrap() else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["flux".to_string()], "{s:?}");
        Store::new(&e.paths.state_dir)
            .load_month("flux", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap()
    }

    fn with_flux(e: &mut Env, label: &str, key: &str) {
        e.config.keys = vec![entry("flux", label)];
        e.keystore
            .put("flux", label, &Secret::new(key.into()))
            .unwrap();
    }

    #[test]
    fn renamed_derived_key_keeps_its_spend_continuous() {
        let mut e = env();
        with_flux(&mut e, "render", "fk-bfl-RENAME0000");
        flux_run(&e, "2026-11-02T09:00:00Z", "1000");
        e.keystore.rename("flux", "render", "studio").unwrap();
        for k in &mut e.config.keys {
            k.label = "studio".into();
        }
        let month = flux_run(&e, "2026-11-03T09:00:00Z", "700");
        let total: Decimal = month.daily.values().flat_map(|m| m.values()).sum();
        assert_eq!(total, Decimal::new(3, 0), "{month:?}");
        let ids: Vec<_> = month.daily.values().flat_map(|m| m.keys()).collect();
        assert_eq!(ids.len(), 1, "{month:?}");
        assert_eq!(month.names.get(ids[0]).map(String::as_str), Some("studio"));
    }

    #[test]
    fn label_reused_after_remove_starts_a_fresh_series() {
        let mut e = env();
        with_flux(&mut e, "render", "fk-bfl-FIRST00000");
        flux_run(&e, "2026-11-02T09:00:00Z", "1000");
        e.keystore.delete("flux", "render").unwrap();
        e.config.keys.clear();
        // Another account under the same label: its lower balance is not spend.
        with_flux(&mut e, "render", "fk-bfl-SECOND0000");
        let month = flux_run(&e, "2026-11-03T09:00:00Z", "400");
        assert!(month.daily.is_empty(), "invented spend: {month:?}");
    }

    #[test]
    fn balance_history_keyed_by_label_still_loads_and_the_fetch_resumes() {
        let mut e = env();
        with_flux(&mut e, "render", "fk-bfl-LABELLED000");
        let dir = e.paths.state_dir.join("balance");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("flux.json"), crate::cache::tests::LABELLED).unwrap();
        let month = flux_run(&e, "2026-11-04T09:00:00Z", "650");
        // The labelled readings are their own series: their drop is kept, none is bridged to the key.
        assert_eq!(
            month.daily[&crate::date::Date::parse("2026-11-03").unwrap()]["render"],
            Decimal::from(3)
        );
        let total: Decimal = month.daily.values().flat_map(|m| m.values()).sum();
        assert_eq!(total, Decimal::from(3), "{month:?}");
        let h = Store::new(&e.paths.state_dir)
            .load_balance("flux")
            .unwrap()
            .unwrap();
        assert_eq!(h.points.len(), 3);
        assert_eq!(h.points[2].series, e.config.keys[0].series);
    }

    #[test]
    fn a_series_minted_for_a_hand_written_entry_is_saved_and_reused() {
        let mut e = env();
        e.keystore
            .put("flux", "render", &Secret::new("fk-bfl-HANDWRITTEN0".into()))
            .unwrap();
        let file = e.paths.config_file();
        let hand_written = "[[key]]\nplatform = \"flux\"\nlabel = \"render\"\n";
        std::fs::create_dir_all(&e.paths.config_dir).unwrap();
        std::fs::write(&file, hand_written).unwrap();
        e.config = Config::load(&file).unwrap();
        flux_run(&e, "2026-11-02T09:00:00Z", "1000");
        let saved = Config::load(&file).unwrap().keys[0].series.clone();
        assert_eq!(saved.len(), 32, "{saved}");
        e.config = Config::load(&file).unwrap();
        let month = flux_run(&e, "2026-11-03T09:00:00Z", "700");
        let h = Store::new(&e.paths.state_dir)
            .load_balance("flux")
            .unwrap()
            .unwrap();
        let series: Vec<_> = h.points.iter().map(|p| p.series.as_str()).collect();
        assert_eq!(
            series,
            [saved.as_str(), saved.as_str()],
            "two fetches, one series"
        );
        assert_eq!(
            month
                .daily
                .values()
                .flat_map(|m| m.values())
                .sum::<Decimal>(),
            Decimal::from(3)
        );

        // Removed and written by hand again under the same label: a fresh series, no invented spend.
        std::fs::write(&file, hand_written).unwrap();
        e.config = Config::load(&file).unwrap();
        let month = flux_run(&e, "2026-11-04T09:00:00Z", "400");
        assert_ne!(Config::load(&file).unwrap().keys[0].series, saved);
        let total: Decimal = month.daily.values().flat_map(|m| m.values()).sum();
        assert_eq!(total, Decimal::from(3), "invented spend: {month:?}");
    }

    /// Succeeds with one warning, or returns before-history from `first`, for the given month.
    struct Scripted {
        warn: bool,
        before: Option<&'static str>,
        credits_warn: bool,
    }

    impl Provider for Scripted {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::Ok("fake".into())
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            if let Some(first) = self.before {
                return Err(crate::providers::before_history(
                    crate::date::Date::parse(first).unwrap(),
                ));
            }
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.add(month.first_day(), "key_1", Decimal::new(125, 2));
            if self.warn {
                c.warnings.push(plain("left out b (removed): bad reading"));
            }
            if self.credits_warn {
                c.credits_warnings
                    .push(plain("key \"a\": credits left could not be computed: x"));
            }
            Ok(c)
        }
    }

    fn scripted(e: &Env, warn: bool, before: Option<&'static str>) -> Summary {
        let registry = Registry::with(vec![Box::new(Scripted {
            warn,
            before,
            credits_warn: false,
        })]);
        let FetchOutcome::Ran(s) = run(&request(e, &registry)).unwrap() else {
            panic!()
        };
        s
    }

    fn log_of(e: &Env) -> Vec<errlog::Entry> {
        errlog::read(&e.paths, &now_rfc3339()).unwrap()
    }

    #[test]
    fn a_failure_and_a_warning_are_logged() {
        let e = env();
        fetch_with(&e, fake(true, false));
        let s = scripted(&e, true, None);
        assert_eq!(s.log_problem, None);
        let log = log_of(&e);
        assert_eq!(log.len(), 2, "{log:?}");
        let failed = log
            .iter()
            .find(|l| l.summary == "Fetch failed for OpenAI")
            .unwrap();
        assert_eq!(failed.platform.as_deref(), Some("openai"));
        assert_eq!(failed.month, Month::parse("2026-11"));
        assert!(
            failed.detail.as_str().contains("invalid key [redacted]"),
            "{failed:?}"
        );
        let warned = log
            .iter()
            .find(|l| l.summary == "Part of OpenAI's spend is missing")
            .unwrap();
        assert_eq!(warned.detail.as_str(), "left out b (removed): bad reading");
        // The same failure an hour later is the same entry.
        fetch_with(&e, fake(true, false));
        let log = log_of(&e);
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(log[0].count, 2);
    }

    #[test]
    fn a_credits_warning_is_logged_under_its_own_title() {
        let e = env();
        let registry = Registry::with(vec![Box::new(Scripted {
            warn: false,
            before: None,
            credits_warn: true,
        })]);
        let FetchOutcome::Ran(s) = run(&request(&e, &registry)).unwrap() else {
            panic!()
        };
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        let log = log_of(&e);
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(
            log[0].summary,
            "OpenAI's credits left could not be computed"
        );
        assert_eq!(
            log[0].detail.as_str(),
            "key \"a\": credits left could not be computed: x"
        );
        let c = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert!(c.warnings.is_empty(), "no spend is missing");
        assert_eq!(c.credits_warnings.len(), 1, "it is saved with the month");
    }

    #[test]
    fn a_requirement_and_a_before_history_month_are_not_logged() {
        let e = env();
        fetch_with(&e, fake(false, true));
        let s = scripted(&e, false, Some("2026-12-01"));
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        assert!(s.failed.is_empty() && s.needs.is_empty(), "{s:?}");
        assert!(log_of(&e).is_empty());
        assert!(!e.paths.errors_file().exists());
        // The requirement's placeholder had no figures, so the month is now a before-history placeholder.
        let c = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(c.history_from, crate::date::Date::parse("2026-12-01"));
        assert!(c.last_fetched.is_some());
        assert!(c.requirement.is_none() && c.last_error.is_none());
        assert!(c.daily.is_empty());
    }

    #[test]
    fn a_before_history_month_keeps_an_earlier_caches_figures_unmarked() {
        let e = env();
        fetch_with(&e, fake(false, false));
        fetch_with(&e, fake(true, false));
        let store = Store::new(&e.paths.state_dir);
        let nov = Month::parse("2026-11").unwrap();
        let before = store.load_month("openai", nov).unwrap().unwrap();
        let s = scripted(&e, false, Some("2026-12-01"));
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        let after = store.load_month("openai", nov).unwrap().unwrap();
        assert_eq!(after.daily, before.daily);
        assert!(after.last_fetched >= before.last_fetched);
        assert_eq!(after.history_from, None);
        assert!(after.last_error.is_none() && after.last_error_at.is_none());
        let r = report_of(&e);
        assert_eq!(r.platforms[0].total, Decimal::new(125, 2));
        assert!(r.platforms[0].last_error.is_none());
    }

    /// A provider whose month is whatever `make` returns.
    struct Made(fn(Month) -> Result<MonthCache, ProviderError>);

    impl Provider for Made {
        fn platform(&self) -> &'static str {
            "openai"
        }
        fn check(&self, _: &dyn Transport, _: &Secret, _: &Config) -> CheckOutcome {
            CheckOutcome::Ok("fake".into())
        }
        fn fetch_month(
            &self,
            _: &dyn Transport,
            _: &[Credential],
            month: Month,
            _: &Config,
        ) -> Result<MonthCache, ProviderError> {
            (self.0)(month)
        }
    }

    fn made(e: &Env, make: fn(Month) -> Result<MonthCache, ProviderError>) {
        let registry = Registry::with(vec![Box::new(Made(make))]);
        run(&request(e, &registry)).unwrap();
    }

    fn day(s: &str) -> crate::date::Date {
        crate::date::Date::parse(s).unwrap()
    }

    #[test]
    fn two_warnings_in_one_fetch_are_one_entry_holding_both() {
        let e = env();
        made(&e, |month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.add(month.first_day(), "key_1", Decimal::ONE);
            c.warnings.push(plain("left out a (removed): bad reading"));
            c.warnings.push(plain("left out b (removed): bad reading"));
            Ok(c)
        });
        let log = log_of(&e);
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].count, 1);
        assert!(log[0].detail.as_str().contains("left out a"), "{log:?}");
        assert!(log[0].detail.as_str().contains("left out b"), "{log:?}");
    }

    #[test]
    fn a_boundary_fetch_keeps_cached_days_before_the_floor() {
        let e = env();
        let nov = Month::parse("2025-11").unwrap();
        let store = Store::new(&e.paths.state_dir);
        let mut old = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        for d in 1..=5 {
            old.add(day(&format!("2025-11-{d:02}")), "key_1", Decimal::ONE);
        }
        old.add(day("2025-11-06"), "key_1", Decimal::from(9));
        old.names.insert("key_1".into(), "work key".into());
        old.last_fetched = Some("2026-11-01T00:00:00Z".into());
        store.save_month(&old).unwrap();
        let registry = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.history_from = Some(crate::date::Date::parse("2025-11-06").unwrap());
            c.add(
                crate::date::Date::parse("2025-11-06").unwrap(),
                "key_1",
                Decimal::from(2),
            );
            Ok(c)
        }))]);
        let mut req = request(&e, &registry);
        req.month = nov;
        run(&req).unwrap();
        let c = store.load_month("openai", nov).unwrap().unwrap();
        let total: Decimal = c.daily.values().flat_map(|m| m.values()).sum();
        assert_eq!(
            total,
            Decimal::from(7),
            "days 1-5 kept, day 6 refetched: {c:?}"
        );
        assert_eq!(
            c.history_from, None,
            "the kept days cover the start of the month"
        );
        assert_eq!(c.names.get("key_1").map(String::as_str), Some("work key"));
    }

    #[test]
    fn a_cache_fetched_mid_month_keeps_the_floor_until_it_reaches_it() {
        let nov = Month::parse("2025-11").unwrap();
        let after = |fetched: &str| {
            let e = env();
            let store = Store::new(&e.paths.state_dir);
            let mut old = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
            old.add(day("2025-11-01"), "key_1", Decimal::ONE);
            old.add(day("2025-11-02"), "key_1", Decimal::ONE);
            old.last_fetched = Some(fetched.into());
            store.save_month(&old).unwrap();
            let registry = Registry::with(vec![Box::new(Made(|month| {
                let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
                c.history_from = Some(day("2025-11-06"));
                Ok(c)
            }))]);
            let mut req = request(&e, &registry);
            req.month = nov;
            run(&req).unwrap();
            store.load_month("openai", nov).unwrap().unwrap()
        };
        let gap = after("2025-11-02T12:00:00Z");
        assert_eq!(gap.daily.len(), 2, "both cached days kept: {gap:?}");
        assert_eq!(
            gap.history_from,
            Some(day("2025-11-06")),
            "November 3 to 5 were never fetched"
        );
        let partial = after("2025-11-05T12:00:00Z");
        assert_eq!(
            partial.history_from,
            Some(day("2025-11-06")),
            "the rest of 2025-11-05 was never fetched"
        );
        let full = after("2025-11-06T00:30:00Z");
        assert_eq!(full.history_from, None, "the old fetch reached the floor");
    }

    #[test]
    fn a_same_day_refetch_of_a_boundary_month_keeps_its_floor() {
        let e = env();
        let nov = Month::parse("2025-11").unwrap();
        let store = Store::new(&e.paths.state_dir);
        let registry = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.history_from = Some(day("2025-11-06"));
            c.add(day("2025-11-07"), "key_1", Decimal::ONE);
            Ok(c)
        }))]);
        let mut req = request(&e, &registry);
        req.month = nov;
        run(&req).unwrap();
        run(&req).unwrap();
        let c = store.load_month("openai", nov).unwrap().unwrap();
        assert_eq!(
            c.history_from,
            Some(day("2025-11-06")),
            "neither fetch reached November 1 to 5"
        );
    }

    #[test]
    fn an_export_start_floor_survives_a_later_fetch_with_rows() {
        let e = env();
        let oct = Month::parse("2026-10").unwrap();
        let store = Store::new(&e.paths.state_dir);
        let empty = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Project);
            c.history_from = Some(day("2026-10-15"));
            Ok(c)
        }))]);
        let rows = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Project);
            c.history_from = Some(day("2026-10-15"));
            c.add(day("2026-10-20"), "proj", Decimal::ONE);
            Ok(c)
        }))]);
        for registry in [&empty, &rows] {
            let mut req = request(&e, registry);
            req.month = oct;
            run(&req).unwrap();
        }
        let c = store.load_month("openai", oct).unwrap().unwrap();
        assert_eq!(c.history_from, Some(day("2026-10-15")), "{c:?}");
    }

    #[test]
    fn a_covered_month_with_no_spend_clears_the_floor() {
        let e = env();
        let nov = Month::parse("2025-11").unwrap();
        let store = Store::new(&e.paths.state_dir);
        let mut old = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        old.last_fetched = Some("2025-11-20T12:00:00Z".into());
        store.save_month(&old).unwrap();
        let registry = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.history_from = Some(day("2025-11-06"));
            Ok(c)
        }))]);
        let mut req = request(&e, &registry);
        req.month = nov;
        run(&req).unwrap();
        let c = store.load_month("openai", nov).unwrap().unwrap();
        assert_eq!(
            c.history_from, None,
            "November 1 to 5 were fetched at $0.00"
        );
    }

    #[test]
    fn an_unreadable_cache_is_not_replaced_by_a_boundary_month() {
        let e = env();
        let nov = Month::parse("2025-11").unwrap();
        let file = e.paths.cache_dir().join("openai").join("2025-11.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{not json").unwrap();
        let registry = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.history_from = Some(crate::date::Date::parse("2025-11-06").unwrap());
            Ok(c)
        }))]);
        let mut req = request(&e, &registry);
        req.month = nov;
        let FetchOutcome::Ran(s) = run(&req).unwrap() else {
            panic!()
        };
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{not json");
        assert!(s.fetched.is_empty(), "{s:?}");
        assert!(s.failed[0].1.contains("could not read the cache"), "{s:?}");
        assert!(
            log_of(&e)[0]
                .detail
                .as_str()
                .contains("could not read the cache"),
            "{:?}",
            log_of(&e)
        );
    }

    #[test]
    fn a_carried_day_never_adds_to_a_fresh_figure() {
        let e = env();
        let nov = Month::parse("2025-11").unwrap();
        let store = Store::new(&e.paths.state_dir);
        let mut old = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        old.add(day("2025-11-03"), "key_1", Decimal::ONE);
        old.add(day("2025-11-04"), "key_1", Decimal::from(3));
        old.last_fetched = Some("2026-11-01T00:00:00Z".into());
        store.save_month(&old).unwrap();
        // A second credential with an older floor already returns 2025-11-03.
        let registry = Registry::with(vec![Box::new(Made(|month| {
            let mut c = MonthCache::new("openai", month, FigureKind::Billed, Breakdown::Key);
            c.history_from = Some(crate::date::Date::parse("2025-11-06").unwrap());
            c.add(
                crate::date::Date::parse("2025-11-03").unwrap(),
                "key_1",
                Decimal::from(5),
            );
            Ok(c)
        }))]);
        let mut req = request(&e, &registry);
        req.month = nov;
        run(&req).unwrap();
        let c = store.load_month("openai", nov).unwrap().unwrap();
        assert_eq!(c.daily[&day("2025-11-03")]["key_1"], Decimal::from(5));
        assert_eq!(c.daily[&day("2025-11-04")]["key_1"], Decimal::from(3));
    }

    #[test]
    fn a_fetched_month_with_no_spend_is_not_a_placeholder() {
        let e = env();
        let nov = Month::parse("2026-11").unwrap();
        let store = Store::new(&e.paths.state_dir);
        let mut old = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
        old.last_fetched = Some("2026-11-01T00:00:00Z".into());
        store.save_month(&old).unwrap();
        scripted(&e, false, Some("2026-12-01"));
        let c = store.load_month("openai", nov).unwrap().unwrap();
        assert_eq!(c.history_from, None, "{c:?}");
        assert!(
            c.last_fetched.as_deref() > Some("2026-11-01T00:00:00Z"),
            "a kept month counts as fetched now, so it is not stale: {c:?}"
        );
        let r = crate::report::build(&store, crate::report::Range::month(nov), None).unwrap();
        assert!(r.platforms[0].before_history.is_empty(), "{r:?}");
        assert!(r.platforms[0].last_fetched.is_some(), "{r:?}");
    }

    #[test]
    fn a_failure_has_one_time_in_the_cache_and_the_log() {
        let e = env();
        fetch_with(&e, fake(true, false));
        let c = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            c.last_error_at.as_deref(),
            Some(log_of(&e)[0].last_at.as_str())
        );
    }

    #[test]
    fn an_earlier_placeholder_takes_the_newer_floor() {
        let e = env();
        scripted(&e, false, Some("2026-12-01"));
        scripted(&e, false, Some("2026-12-02"));
        let c = Store::new(&e.paths.state_dir)
            .load_month("openai", Month::parse("2026-11").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(c.history_from, Some(day("2026-12-02")));
    }

    #[test]
    fn a_locked_keyring_is_logged_with_no_platform() {
        let e = env();
        e.keystore.set_locked(true);
        assert!(
            run(&request(
                &e,
                &Registry::with(vec![Box::new(fake(false, false))])
            ))
            .is_err()
        );
        let log = log_of(&e);
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].platform, None);
        assert_eq!(log[0].month, None);
        assert_eq!(log[0].summary, "Keyring locked · unlock it, then Refresh");
        assert!(
            log[0].detail.as_str().starts_with("keyring locked"),
            "{log:?}"
        );
    }

    #[test]
    fn a_log_write_failure_is_a_log_problem_not_a_failed_fetch() {
        let e = env();
        // A directory where the log lock belongs makes every log write fail.
        std::fs::create_dir_all(e.paths.log_lock_file()).unwrap();
        let s = scripted(&e, true, None);
        assert_eq!(s.fetched, vec!["openai".to_string()], "{s:?}");
        assert!(s.failed.is_empty(), "the exit code stays 0: {s:?}");
        let problem = s.log_problem.expect("log problem");
        assert!(
            problem.contains("could not write the error log"),
            "{problem}"
        );
    }

    /// Answers every request with a 401 whose body holds the request's key, bypassing `Guarded`'s scrub.
    struct Echo401;
    impl Transport for Echo401 {
        fn send(&self, _: &Request, credential: &Secret) -> Result<Response, HttpError> {
            Err(HttpError::Status {
                status: 401,
                body: plain(format!(
                    r#"{{"error":{{"message":"Incorrect API key provided: {}"}}}}"#,
                    credential.expose()
                )),
            })
        }
    }

    #[test]
    fn a_rejected_key_never_reaches_the_error_log() {
        let e = env();
        let registry = Registry::with(vec![Box::new(crate::providers::openai::OpenAi {
            today: || crate::date::Date::parse("2026-11-05").unwrap(),
        })]);
        let mut r = request(&e, &registry);
        r.transport = &Echo401;
        let FetchOutcome::Ran(s) = run(&r).unwrap() else {
            panic!()
        };
        assert_eq!(s.failed.len(), 1, "{s:?}");
        let file = std::fs::read_to_string(e.paths.errors_file()).unwrap();
        assert!(!file.contains(KEY), "{file}");
        assert!(!file.contains(&KEY[..8]), "{file}");
        assert!(file.contains("[redacted]"), "{file}");
        assert!(
            file.contains("Key rejected · check the key in the app"),
            "{file}"
        );
    }

    /// A raw wire answering a 401 whose body, cut short so it is not valid JSON, writes the key in `\uXXXX` escapes.
    struct EscapedEcho401;
    impl crate::http::RawTransport for EscapedEcho401 {
        fn send_raw(&self, _: &ureq::http::Uri, _: &Request) -> Result<Response, String> {
            let escaped: String = KEY
                .chars()
                .map(|c| format!("\\u{:04x}", c as u32))
                .collect();
            Ok(Response {
                status: 401,
                body: format!(r#"{{"error":{{"message":"Incorrect API key provided: {escaped}"#)
                    .into_bytes(),
            })
        }
    }

    /// A raw wire answering a 401 whose body writes the key in `\uXXXX` escapes under three more layers of escaped backslashes.
    struct DeepEscaped401;
    impl crate::http::RawTransport for DeepEscaped401 {
        fn send_raw(&self, _: &ureq::http::Uri, _: &Request) -> Result<Response, String> {
            let deep: String = KEY
                .chars()
                .map(|c| format!("{}u{:04x}", "\\".repeat(8), c as u32))
                .collect();
            Ok(Response {
                status: 401,
                body: format!(r#"{{"error":{{"message":"Incorrect API key provided: {deep}"}}}}"#)
                    .into_bytes(),
            })
        }
    }

    #[test]
    fn a_key_escaped_past_the_decode_levels_never_reaches_cache_log_or_output() {
        let e = env();
        let registry = Registry::with(vec![Box::new(crate::providers::openai::OpenAi {
            today: || crate::date::Date::parse("2026-11-05").unwrap(),
        })]);
        let guarded = crate::http::Guarded::new(DeepEscaped401);
        let mut r = request(&e, &registry);
        r.transport = &guarded;
        let FetchOutcome::Ran(s) = run(&r).unwrap() else {
            panic!()
        };
        assert_eq!(s.failed.len(), 1, "{s:?}");
        let cache =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        let log = std::fs::read_to_string(e.paths.errors_file()).unwrap();
        for text in [&s.failed[0].1, &cache, &log] {
            assert!(!text.contains("u0066"), "{text}");
            assert!(text.contains("too heavily escaped"), "{text}");
        }
    }

    #[test]
    fn a_u_escaped_key_in_a_malformed_body_never_reaches_cache_log_or_output() {
        let e = env();
        let registry = Registry::with(vec![Box::new(crate::providers::openai::OpenAi {
            today: || crate::date::Date::parse("2026-11-05").unwrap(),
        })]);
        let guarded = crate::http::Guarded::new(EscapedEcho401);
        let mut r = request(&e, &registry);
        r.transport = &guarded;
        let FetchOutcome::Ran(s) = run(&r).unwrap() else {
            panic!()
        };
        assert_eq!(s.failed.len(), 1, "{s:?}");
        let head: String = KEY[..8]
            .chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect();
        let cache =
            std::fs::read_to_string(e.paths.state_dir.join("cache/openai/2026-11.json")).unwrap();
        let log = std::fs::read_to_string(e.paths.errors_file()).unwrap();
        for text in [&s.failed[0].1, &cache, &log] {
            assert!(!text.contains(KEY) && !text.contains(&KEY[..8]), "{text}");
            assert!(!text.contains(&head), "{text}");
            assert!(text.contains("[redacted]"), "{text}");
        }
    }

    #[test]
    fn zero_providers_reports_no_provider() {
        let e = env();
        let FetchOutcome::Ran(s) = run(&request(&e, &Registry::default())).unwrap() else {
            panic!()
        };
        assert_eq!(s.no_provider, vec!["openai".to_string()]);
    }
}
