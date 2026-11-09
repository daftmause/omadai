//! The persisted error log `<state>/errors.json`, which the app opens in place of raw error text.

use crate::date::{Date, Month};
use crate::error::Error;
use crate::fsutil;
use crate::keystore::KeystoreError;
use crate::lock;
use crate::paths::Paths;
use crate::provider::{Cause, ProviderError};
use crate::scrub::{Scrubbed, plain, printable};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

pub const LOG_VERSION: u32 = 1;
/// Entries whose last occurrence is older than this many days are dropped on every write.
pub const KEEP_DAYS: i64 = 30;
pub const MAX_ENTRIES: usize = 200;
/// The bound on a detail from a source other than `http.rs`, which caps its own text.
pub const MAX_DETAIL: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub first_at: String,
    pub last_at: String,
    pub count: u32,
    pub platform: Option<String>,
    pub month: Option<Month>,
    /// omadai's own words for the failure.
    pub summary: String,
    pub detail: Scrubbed,
}

#[derive(Debug, Serialize, Deserialize)]
struct LogFile {
    version: u32,
    entries: Vec<Entry>,
}

/// One failure to record; `fetch::run` builds these and writes them in one hold of `log.lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub platform: Option<String>,
    pub month: Option<Month>,
    pub summary: String,
    pub detail: Scrubbed,
}

impl Record {
    pub fn new(
        platform: Option<&str>,
        month: Option<Month>,
        summary: String,
        detail: Scrubbed,
    ) -> Self {
        Record {
            platform: platform.map(str::to_string),
            month,
            summary,
            detail: detail.truncated(MAX_DETAIL),
        }
    }
}

/// What failed: a platform's fetch, or the whole run.
pub enum Failure<'a> {
    Provider(&'a ProviderError),
    Run(&'a Error),
}

/// omadai's summary of a failure; `name` is the platform's display name, absent for the whole run.
pub fn summary(failure: &Failure<'_>, name: Option<&str>) -> String {
    let named = |text: &dyn Fn(&str) -> String| match name {
        Some(n) => text(n),
        None => text("the platform"),
    };
    let cause = match failure {
        Failure::Run(Error::Keystore(KeystoreError::Locked)) => {
            return "Keyring locked · unlock it, then Refresh".into();
        }
        Failure::Run(_) => None,
        Failure::Provider(e) => Some(e.cause),
    };
    match cause {
        Some(Cause::Status(429)) => {
            named(&|n| format!("Rate limited by {n} · the next fetch tries again"))
        }
        Some(Cause::Status(401 | 403)) => "Key rejected · check the key in the app".into(),
        Some(Cause::Status(500..)) => {
            named(&|n| format!("{n} had a server error · the next fetch tries again"))
        }
        Some(Cause::Transport) => {
            named(&|n| format!("Could not reach {n} · the next fetch tries again"))
        }
        Some(Cause::MissingCredential) => {
            "A key has no stored secret · add it again in the app".into()
        }
        Some(Cause::SignInExpired) => {
            "Google sign-in expired or was revoked · remove the Gemini key and add it again".into()
        }
        Some(Cause::SessionPolicy) => "Your Google Workspace makes Google Cloud apps sign in again on a timer · an admin marks omadai Trusted in App access control and ticks Exempt trusted apps in Google Cloud session control, then remove the Gemini key and add it again".into(),
        Some(Cause::ClientGone) => {
            "The Google OAuth client was deleted or rotated · remove the Gemini key and add it again"
                .into()
        }
        _ => match name {
            Some(n) => format!("Fetch failed for {n}"),
            None => "Fetch failed".into(),
        },
    }
}

/// The record for a run that failed as a whole; `Error` carries no credential by construction.
pub fn run_record(e: &Error) -> Record {
    Record::new(
        None,
        None,
        summary(&Failure::Run(e), None),
        plain(e.to_string()),
    )
}

/// `now` less `KEEP_DAYS`, in the same RFC 3339 form, so the two compare as strings.
fn cutoff(now: &str) -> Option<String> {
    let day = Date::parse(now.get(..10)?)?;
    Some(format!(
        "{}{}",
        Date::from_days(day.to_days() - KEEP_DAYS),
        &now[10..]
    ))
}

/// Newest first, the stale dropped, at most `MAX_ENTRIES`.
fn pruned(mut entries: Vec<Entry>, now: &str) -> Vec<Entry> {
    entries.sort_by(|a, b| b.last_at.cmp(&a.last_at));
    if let Some(cut) = cutoff(now) {
        entries.retain(|e| e.last_at >= cut);
    }
    entries.truncate(MAX_ENTRIES);
    entries
}

fn parse(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let file: LogFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if file.version != LOG_VERSION {
        return Err(format!("unknown version {}", file.version));
    }
    Ok(file.entries)
}

fn write(paths: &Paths, entries: Vec<Entry>) -> Result<(), Error> {
    let file = LogFile {
        version: LOG_VERSION,
        entries,
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| Error::invalid(format!("serialize the error log: {e}")))?;
    fsutil::atomic_write(&paths.errors_file(), &bytes)
}

/// Adds `records` to the log under `log.lock`; `Ok(Some(_))` says an unreadable log was moved aside.
pub fn record(paths: &Paths, records: Vec<Record>, now: &str) -> Result<Option<String>, Error> {
    if records.is_empty() {
        return Ok(None);
    }
    let _log = lock::acquire(&paths.log_lock_file())?;
    let path = paths.errors_file();
    let mut moved = None;
    // Only a file that reads but does not parse is moved aside; a failed read leaves it in place.
    let mut entries = match fsutil::read_optional(&path)? {
        None => Vec::new(),
        Some(bytes) => match parse(&bytes) {
            Ok(entries) => entries,
            Err(why) => {
                moved = Some(move_aside(paths, &why, now)?);
                Vec::new()
            }
        },
    };
    for r in records {
        match entries
            .iter_mut()
            .find(|e| e.platform == r.platform && e.month == r.month && e.summary == r.summary)
        {
            Some(e) => {
                e.last_at = now.to_string();
                e.count = e.count.saturating_add(1);
                e.detail = r.detail;
            }
            None => entries.push(Entry {
                first_at: now.to_string(),
                last_at: now.to_string(),
                count: 1,
                platform: r.platform,
                month: r.month,
                summary: r.summary,
                detail: r.detail,
            }),
        }
    }
    write(paths, pruned(entries, now))?;
    Ok(moved)
}

/// Moves the log to a name stamped with `now`, so an earlier unreadable log is never overwritten.
fn move_aside(paths: &Paths, why: &str, now: &str) -> Result<String, Error> {
    let path = paths.errors_file();
    let stamp: String = now.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let bad = path.with_extension(format!("json.bad-{stamp}"));
    std::fs::rename(&path, &bad)
        .map_err(|e| Error::io(format!("move {} aside", path.display()), e))?;
    Ok(format!(
        "the error log could not be read ({why}); it was moved to {} and a new one started",
        bad.display()
    ))
}

/// The log as kept at `now`, newest first; reads only, so a corrupt file is an error here.
pub fn read(paths: &Paths, now: &str) -> Result<Vec<Entry>, Error> {
    match fsutil::read_optional(&paths.errors_file())? {
        None => Ok(Vec::new()),
        Some(bytes) => parse(&bytes)
            .map(|entries| pruned(entries, now))
            .map_err(|why| Error::invalid(format!("the error log could not be read: {why}"))),
    }
}

/// Empties the log under `log.lock`, moving an unparsable one aside first as `record` does.
pub fn clear(paths: &Paths, now: &str) -> Result<Option<String>, Error> {
    let _log = lock::acquire(&paths.log_lock_file())?;
    let moved = match fsutil::read_optional(&paths.errors_file())? {
        Some(bytes) => match parse(&bytes) {
            Ok(_) => None,
            Err(why) => Some(move_aside(paths, &why, now)?),
        },
        None => None,
    };
    write(paths, Vec::new())?;
    Ok(moved)
}

/// One line per entry, then its detail indented; a null platform or month shows as `-`.
pub fn to_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        let month = e.month.map_or("-".to_string(), |m| m.to_string());
        let _ = writeln!(
            out,
            "{} {} {} ×{} {}",
            e.last_at,
            printable(e.platform.as_deref().unwrap_or("-")),
            month,
            e.count,
            printable(&e.summary)
        );
        for line in e.detail.as_str().lines() {
            let _ = writeln!(out, "    {}", printable(line));
        }
    }
    out
}

pub fn to_json(entries: &[Entry]) -> String {
    serde_json::to_string(entries).expect("entries serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TempDir;
    use crate::providers::{fail, fetch_each, testutil::cred};

    fn paths(t: &TempDir) -> Paths {
        Paths {
            home: Some(t.path().join("home")),
            config_dir: t.path().join("config"),
            state_dir: t.path().join("state"),
            keyrings_dir: t.path().join("keyrings"),
        }
    }

    fn rec(platform: &str, summary: &str, detail: &str) -> Record {
        Record::new(
            Some(platform),
            Month::parse("2026-11"),
            summary.into(),
            plain(detail),
        )
    }

    fn at(day: &str) -> String {
        format!("{day}T10:00:00Z")
    }

    #[test]
    fn a_repeat_coalesces_into_one_entry() {
        let t = TempDir::new("errlog-coalesce");
        let p = paths(&t);
        record(
            &p,
            vec![rec("openai", "Fetch failed for OpenAI", "first")],
            &at("2026-11-01"),
        )
        .unwrap();
        record(
            &p,
            vec![rec("openai", "Fetch failed for OpenAI", "second")],
            &at("2026-11-02"),
        )
        .unwrap();
        record(
            &p,
            vec![rec("xai", "Fetch failed for xAI", "other")],
            &at("2026-11-02"),
        )
        .unwrap();
        let mut other_month = rec("openai", "Fetch failed for OpenAI", "october");
        other_month.month = Month::parse("2026-10");
        record(&p, vec![other_month], &at("2026-11-03")).unwrap();
        let entries = read(&p, &at("2026-11-03")).unwrap();
        assert_eq!(entries.len(), 3, "{entries:?}");
        let openai = entries
            .iter()
            .find(|e| e.platform.as_deref() == Some("openai") && e.month == Month::parse("2026-11"))
            .unwrap();
        assert_eq!(openai.count, 2);
        assert_eq!(openai.first_at, at("2026-11-01"));
        assert_eq!(openai.last_at, at("2026-11-02"));
        assert_eq!(openai.detail.as_str(), "second");
        assert_eq!(entries[0].detail.as_str(), "october", "newest first");
        assert_eq!(fsutil::mode_of(&p.errors_file()), Some(0o600));
    }

    #[test]
    fn entries_over_thirty_days_old_are_pruned() {
        let t = TempDir::new("errlog-prune");
        let p = paths(&t);
        record(&p, vec![rec("openai", "old", "x")], &at("2026-10-01")).unwrap();
        record(&p, vec![rec("xai", "edge", "x")], &at("2026-10-07")).unwrap();
        record(&p, vec![rec("fal", "new", "x")], &at("2026-11-06")).unwrap();
        let file = std::fs::read_to_string(p.errors_file()).unwrap();
        assert!(!file.contains("\"old\""), "{file}");
        let entries = read(&p, &at("2026-11-06")).unwrap();
        let kept: Vec<_> = entries.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(kept, ["new", "edge"], "exactly 30 days old is kept");
        // Reading at a later clock hides what a write at that clock would drop.
        let later = read(&p, &at("2026-11-07")).unwrap();
        assert_eq!(later.len(), 1);
    }

    #[test]
    fn the_log_keeps_at_most_two_hundred_newest_entries() {
        let t = TempDir::new("errlog-cap");
        let p = paths(&t);
        let records: Vec<_> = (0..250)
            .map(|i| rec("openai", &format!("failure {i:03}"), "x"))
            .collect();
        record(&p, records, &at("2026-11-01")).unwrap();
        record(&p, vec![rec("xai", "newest", "x")], &at("2026-11-02")).unwrap();
        let entries = read(&p, &at("2026-11-02")).unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].summary, "newest");
    }

    #[test]
    fn a_corrupt_log_is_moved_aside_and_a_fresh_one_starts() {
        let t = TempDir::new("errlog-corrupt");
        let p = paths(&t);
        std::fs::create_dir_all(&p.state_dir).unwrap();
        std::fs::write(p.errors_file(), "not json").unwrap();
        assert!(
            read(&p, &at("2026-11-01")).is_err(),
            "a read changes nothing"
        );
        assert_eq!(std::fs::read(p.errors_file()).unwrap(), b"not json");
        let moved = record(
            &p,
            vec![rec("openai", "Fetch failed for OpenAI", "x")],
            &at("2026-11-01"),
        )
        .unwrap()
        .expect("the move is reported");
        assert!(
            moved.contains("errors.json.bad-20261101T100000Z"),
            "{moved}"
        );
        assert_eq!(
            std::fs::read(p.state_dir.join("errors.json.bad-20261101T100000Z")).unwrap(),
            b"not json"
        );
        assert_eq!(read(&p, &at("2026-11-01")).unwrap().len(), 1);
    }

    #[test]
    fn clear_moves_a_corrupt_log_aside_first() {
        let t = TempDir::new("errlog-clear-corrupt");
        let p = paths(&t);
        std::fs::create_dir_all(&p.state_dir).unwrap();
        std::fs::write(p.errors_file(), "{not json").unwrap();
        let moved = clear(&p, &at("2026-11-01")).unwrap();
        assert!(moved.is_some_and(|m| m.contains("errors.json.bad-20261101T100000Z")));
        assert_eq!(
            std::fs::read_to_string(p.state_dir.join("errors.json.bad-20261101T100000Z")).unwrap(),
            "{not json"
        );
        assert!(read(&p, &at("2026-11-01")).unwrap().is_empty());
    }

    #[test]
    fn a_second_corrupt_log_never_overwrites_the_first() {
        let t = TempDir::new("errlog-corrupt-twice");
        let p = paths(&t);
        std::fs::create_dir_all(&p.state_dir).unwrap();
        for (day, body) in [("2026-11-01", "first"), ("2026-11-02", "second")] {
            std::fs::write(p.errors_file(), body).unwrap();
            record(
                &p,
                vec![rec("openai", "Fetch failed for OpenAI", "x")],
                &at(day),
            )
            .unwrap();
        }
        let kept = |stamp: &str| {
            std::fs::read_to_string(p.state_dir.join(format!("errors.json.bad-{stamp}"))).unwrap()
        };
        assert_eq!(kept("20261101T100000Z"), "first");
        assert_eq!(kept("20261102T100000Z"), "second");
    }

    #[test]
    fn a_log_that_cannot_be_read_stays_where_it_is() {
        let t = TempDir::new("errlog-unreadable");
        let p = paths(&t);
        // A directory in the log's place fails the read itself, not the parse.
        std::fs::create_dir_all(p.errors_file()).unwrap();
        let r = record(
            &p,
            vec![rec("openai", "Fetch failed for OpenAI", "x")],
            &at("2026-11-01"),
        );
        assert!(r.is_err(), "{r:?}");
        assert!(p.errors_file().is_dir(), "left in place");
        let others: Vec<_> = std::fs::read_dir(&p.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.contains(".bad"))
            .collect();
        assert!(others.is_empty(), "{others:?}");
    }

    #[test]
    fn clear_and_record_under_the_log_lock_lose_no_update() {
        let t = TempDir::new("errlog-race");
        let p = paths(&t);
        const N: usize = 40;
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|s| {
            for side in ["a", "b"] {
                let (p, barrier) = (&p, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    for i in 0..N {
                        record(
                            p,
                            vec![rec(side, &format!("{i:02}"), "x")],
                            &at("2026-11-01"),
                        )
                        .unwrap();
                    }
                });
            }
            let (p, barrier) = (&p, &barrier);
            s.spawn(move || {
                barrier.wait();
                std::thread::sleep(std::time::Duration::from_millis(5));
                clear(p, &at("2026-11-01")).unwrap();
            });
        });
        let entries = read(&p, &at("2026-11-01")).unwrap();
        // Each writer's surviving entries are exactly those it wrote after the clear: a tail.
        for side in ["a", "b"] {
            let mut kept: Vec<usize> = entries
                .iter()
                .filter(|e| e.platform.as_deref() == Some(side))
                .map(|e| e.summary.parse().unwrap())
                .collect();
            kept.sort();
            let tail: Vec<usize> = (N - kept.len()..N).collect();
            assert_eq!(kept, tail, "{side} lost an update");
        }
        assert!(entries.iter().all(|e| e.count == 1));

        // A clear waits for a writer holding the log lock.
        let held = lock::acquire(&p.log_lock_file()).unwrap();
        let start = std::time::Instant::now();
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                clear(&p, &at("2026-11-01")).unwrap();
                start.elapsed()
            });
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
            assert!(waiter.join().unwrap() >= std::time::Duration::from_millis(200));
        });
        assert!(read(&p, &at("2026-11-01")).unwrap().is_empty());
    }

    fn provider_err(cause: Cause) -> ProviderError {
        ProviderError { cause, ..fail("x") }
    }

    #[test]
    fn summaries_name_the_cause_in_omadais_words() {
        let s = |cause| summary(&Failure::Provider(&provider_err(cause)), Some("OpenAI"));
        assert_eq!(
            s(Cause::Status(429)),
            "Rate limited by OpenAI · the next fetch tries again"
        );
        assert_eq!(
            s(Cause::Status(401)),
            "Key rejected · check the key in the app"
        );
        assert_eq!(
            s(Cause::Status(403)),
            "Key rejected · check the key in the app"
        );
        for status in [500, 502, 503] {
            assert_eq!(
                s(Cause::Status(status)),
                "OpenAI had a server error · the next fetch tries again"
            );
        }
        assert_eq!(
            s(Cause::Transport),
            "Could not reach OpenAI · the next fetch tries again"
        );
        assert_eq!(
            s(Cause::MissingCredential),
            "A key has no stored secret · add it again in the app"
        );
        assert_eq!(
            s(Cause::SignInExpired),
            "Google sign-in expired or was revoked · remove the Gemini key and add it again"
        );
        assert_eq!(
            s(Cause::SessionPolicy),
            "Your Google Workspace makes Google Cloud apps sign in again on a timer · an admin marks omadai Trusted in App access control and ticks Exempt trusted apps in Google Cloud session control, then remove the Gemini key and add it again"
        );
        assert_eq!(
            s(Cause::ClientGone),
            "The Google OAuth client was deleted or rotated · remove the Gemini key and add it again"
        );
        for other in [
            Cause::Status(400),
            Cause::Redirect(302),
            Cause::Refused,
            Cause::Other,
        ] {
            assert_eq!(s(other), "Fetch failed for OpenAI");
        }
        let locked = Error::Keystore(KeystoreError::Locked);
        assert_eq!(
            summary(&Failure::Run(&locked), None),
            "Keyring locked · unlock it, then Refresh"
        );
        let io = Error::invalid("disk full");
        assert_eq!(summary(&Failure::Run(&io), None), "Fetch failed");
        assert_eq!(run_record(&io).platform, None);
        assert_eq!(run_record(&io).detail.as_str(), "disk full");
    }

    #[test]
    fn a_labelled_error_keeps_its_cause() {
        let e = fetch_each(&[cred("work", "fk-label-000000000000")], |_| {
            Err(provider_err(Cause::Status(429)))
        })
        .unwrap_err();
        assert!(
            e.message.as_str().starts_with("key \"work\": "),
            "{}",
            e.message
        );
        assert_eq!(e.cause, Cause::Status(429));
        assert_eq!(
            summary(&Failure::Provider(&e), Some("OpenAI")),
            "Rate limited by OpenAI · the next fetch tries again"
        );
        let first = crate::date::Date::parse("2026-01-01").unwrap();
        let e = fetch_each(&[cred("work", "fk-label-000000000000")], |_| {
            Err(crate::providers::before_history(first))
        })
        .unwrap_err();
        assert_eq!(e.before_history, Some(first));
    }

    #[test]
    fn text_has_one_line_per_entry_then_the_detail() {
        let e = Entry {
            first_at: at("2026-11-01"),
            last_at: at("2026-11-02"),
            count: 3,
            platform: None,
            month: None,
            summary: "Fetch failed".into(),
            detail: plain("line one\nline two"),
        };
        assert_eq!(
            to_text(&[e]),
            "2026-11-02T10:00:00Z - - ×3 Fetch failed\n    line one\n    line two\n"
        );
    }

    #[test]
    fn to_text_escapes_ansi_sequences_and_to_json_keeps_them() {
        let entry = Entry {
            first_at: at("2026-11-01"),
            last_at: at("2026-11-01"),
            count: 1,
            platform: Some("openai\u{1b}[31m".into()),
            month: Month::parse("2026-11"),
            summary: "Fetch failed \u{1b}[2J".into(),
            detail: plain("HTTP 500 \u{1b}[31mred\u{9b}0m\nsecond \u{7f}line"),
        };
        let text = to_text(std::slice::from_ref(&entry));
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        assert!(text.contains("openai\\u{1b}[31m"), "{text}");
        assert!(text.contains("Fetch failed \\u{1b}[2J"), "{text}");
        assert!(
            text.contains("    HTTP 500 \\u{1b}[31mred\\u{9b}0m\n    second \\u{7f}line"),
            "{text}"
        );
        let back: Vec<Entry> =
            serde_json::from_str(&to_json(std::slice::from_ref(&entry))).unwrap();
        assert_eq!(back, vec![entry]);
    }
}
