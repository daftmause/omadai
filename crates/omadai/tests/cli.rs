//! End-to-end checks of the `omadai` binary in a sandboxed HOME, with the test build's in-memory keystore.

use omadai_core::cache::{Breakdown, FigureKind, MonthCache, Store};
use omadai_core::date::{Date, Month};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn dec(s: &str) -> omadai_core::Decimal {
    s.parse().unwrap()
}

const BIN: &str = env!("CARGO_BIN_EXE_omadai");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "omadai-cli-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        let s = Sandbox { root };
        let out = s.run(&["doctor"]);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("keystore: in-memory"),
            "test build is not using the in-memory keystore:\n{text}"
        );
        s
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.root.join("home")),
            ("XDG_CONFIG_HOME", self.root.join("config")),
            ("XDG_STATE_HOME", self.root.join("state")),
            ("XDG_DATA_HOME", self.root.join("data")),
            // Defence in depth: even a build without the fake could not reach the real keyring.
            (
                "DBUS_SESSION_BUS_ADDRESS",
                PathBuf::from("unix:path=/nonexistent/omadai-test"),
            ),
        ]
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(self.env())
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn state(&self) -> PathBuf {
        self.root.join("state").join("omadai")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config").join("omadai").join("config.toml")
    }

    fn unencrypted_keyring(&self) {
        let dir = self.root.join("data").join("keyrings");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("default"), "login\n").unwrap();
        fs::write(
            dir.join("login.keyring"),
            "[keyring]\ndisplay-name=Login\nctime=0\n",
        )
        .unwrap();
    }

    fn write_config(&self, text: &str) {
        fs::create_dir_all(self.config_file().parent().unwrap()).unwrap();
        fs::write(self.config_file(), text).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn all_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[test]
fn usage_errors_exit_2() {
    let s = Sandbox::new("usage");
    for args in [
        &["nope"][..],
        &["report", "--month", "2026-13"],
        &["report", "--from", "2026-11-01"],
        &["fetch", "--platform", "nope"],
        &["key", "add"],
    ] {
        let out = s.run(args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn unencrypted_keyring_refuses_without_flag() {
    let s = Sandbox::new("unencrypted");
    s.unencrypted_keyring();
    let out = s.run(&["key", "add", "openai", "--label", "work"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("refusing to store a credential"),
        "{stderr}"
    );
    assert!(stderr.contains("--allow-unencrypted-keyring"), "{stderr}");
    assert!(
        !s.config_file().exists(),
        "nothing may be recorded on refusal"
    );

    let doctor = String::from_utf8_lossy(&s.run(&["doctor"]).stdout).to_string();
    assert!(doctor.contains("UNENCRYPTED"), "{doctor}");

    // With the flag, the refusal is passed and it goes on to need a terminal for the hidden prompt.
    let out = s.run(&[
        "key",
        "add",
        "openai",
        "--label",
        "work",
        "--allow-unencrypted-keyring",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("refusing"), "{stderr}");
    assert!(stderr.contains("needs a terminal"), "{stderr}");
}

#[test]
fn fetch_exits_at_once_while_another_holds_the_lock() {
    let s = Sandbox::new("lock");
    s.write_config("[[key]]\nplatform = \"openai\"\nlabel = \"work\"\n");
    fs::create_dir_all(s.state()).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(s.state().join("lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let start = Instant::now();
    let children: Vec<_> = (0..10)
        .map(|_| s.cmd(&["fetch"]).stderr(Stdio::piped()).spawn().unwrap())
        .collect();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stderr).contains("another fetch is running"));
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "contenders took {:?}",
        start.elapsed()
    );
    drop(lock);

    let children: Vec<_> = (0..10)
        .map(|_| s.cmd(&["fetch"]).stderr(Stdio::piped()).spawn().unwrap())
        .collect();
    let mut ran = 0;
    for c in children {
        let out = c.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("another fetch is running") {
            assert_eq!(out.status.code(), Some(0), "{err}");
        } else {
            // The label is configured but the test keystore holds no credential for it.
            assert_eq!(out.status.code(), Some(1), "{err}");
            assert!(
                err.contains("openai: a key label is configured but no credential is stored"),
                "{err}"
            );
            ran += 1;
        }
    }
    assert!(ran >= 1);
    assert_eq!(mode(&s.state()), 0o700);
    assert_eq!(mode(&s.state().join("lock")), 0o600);
}

#[test]
fn report_reads_cache_and_state_is_private() {
    let s = Sandbox::new("report");
    let store = Store::new(&s.state());
    let nov = Month::parse("2026-11").unwrap();
    let mut c = MonthCache::new("openai", nov, FigureKind::Billed, Breakdown::Key);
    c.add(
        Date::parse("2026-11-03").unwrap(),
        "key_a",
        dec("12345678901234.56789"),
    );
    c.add(Date::parse("2026-11-04").unwrap(), "key_a", dec("0.1"));
    store.save_month(&c).unwrap();

    let out = s.run(&["report", "--month", "2026-11", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["total"], "12345678901234.66789");
    assert_eq!(v["platforms"][0]["items"][0]["id"], "key_a");

    let out = s.run(&[
        "report",
        "--from",
        "2026-11-04",
        "--to",
        "2026-11-30",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["total"], "0.1");

    let text =
        String::from_utf8_lossy(&s.run(&["report", "--month", "2026-11"]).stdout).to_string();
    assert!(text.contains("OpenAI"), "{text}");

    let mut files = Vec::new();
    all_files(&s.state(), &mut files);
    assert!(!files.is_empty());
    for f in files {
        assert_eq!(mode(&f), 0o600, "{}", f.display());
    }
    assert_eq!(mode(&s.state()), 0o700);
    let doctor = s.run(&["doctor"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
}

#[test]
fn key_list_rename_remove_touch_labels_only() {
    let s = Sandbox::new("labels");
    s.write_config("[[key]]\nplatform = \"openai\"\nlabel = \"work\"\n\n[[key]]\nplatform = \"xai\"\nlabel = \"team\"\n");
    let list = String::from_utf8_lossy(&s.run(&["key", "list"]).stdout).to_string();
    assert_eq!(list, "openai\twork\nxai\tteam\n");
    let json: serde_json::Value =
        serde_json::from_slice(&s.run(&["key", "list", "--json"]).stdout).unwrap();
    assert_eq!(json[1]["label"], "team");

    // Each run has its own empty in-memory keystore, so rename finds no credential and changes nothing.
    let out = s.run(&["key", "rename", "openai", "work", "home"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&s.run(&["key", "list"]).stdout).contains("openai\twork"));

    let out = s.run(&["key", "remove", "xai", "team"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&s.run(&["key", "list"]).stdout),
        "openai\twork\n"
    );
    assert_eq!(mode(&s.config_file()), 0o600);
    assert_eq!(
        s.run(&["key", "remove", "xai", "team"]).status.code(),
        Some(1)
    );
}

#[test]
fn key_remove_waits_for_another_key_command_and_reloads_the_config() {
    let s = Sandbox::new("keys-lock");
    s.write_config("[[key]]\nplatform = \"openai\"\nlabel = \"work\"\n");
    fs::create_dir_all(s.state()).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(s.state().join("keys.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut child = s
        .cmd(&["key", "remove", "openai", "work"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "key remove ran while another key command held the lock"
    );
    // What the holder saves before releasing is what the waiting command must build on.
    s.write_config(
        "[[key]]\nplatform = \"openai\"\nlabel = \"work\"\n\n[[key]]\nplatform = \"xai\"\nlabel = \"team\"\n",
    );
    drop(lock);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&s.run(&["key", "list"]).stdout),
        "xai\tteam\n"
    );
}

/// Drives `key add` through a real pty with util-linux `script`, typing the key only after the prompt shows.
#[cfg(target_os = "linux")]
#[test]
fn key_add_through_pty_never_exposes_the_key() {
    const KEY: &str = "fk-admin-PTYTESTKEY24680ACEGIKM";
    let s = Sandbox::new("pty");
    s.unencrypted_keyring();
    let command = format!("'{BIN}' key add openai --label pty --allow-unencrypted-keyring");
    let mut child = Command::new("script")
        .args(["-q", "-c", &command, "/dev/null"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(s.env())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("util-linux script is needed for this test");
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let output = output.clone();
        let mut stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                output.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    let wait_for = |needle: &str| {
        let start = Instant::now();
        while !String::from_utf8_lossy(&output.lock().unwrap()).contains(needle) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out waiting for {needle:?}: {}",
                String::from_utf8_lossy(&output.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let mut stdin = child.stdin.take().unwrap();
    wait_for("(input hidden): ");
    // rpassword prints the prompt before it turns echo off; give it time so the test models a person typing.
    std::thread::sleep(Duration::from_millis(300));
    stdin.write_all(format!("{KEY}\n").as_bytes()).unwrap();
    wait_for("Press Enter to close.");

    let pid = find_descendant(child.id(), "omadai").expect("omadai process under script");
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
    for (what, bytes) in [("cmdline", &cmdline), ("environ", &environ)] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(KEY),
            "key in /proc/{pid}/{what}"
        );
        assert!(
            !String::from_utf8_lossy(bytes).contains("PTYTESTKEY"),
            "key fragment in /proc/{pid}/{what}"
        );
    }
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.lock().unwrap()).to_string();
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("Stored OpenAI key \"pty\""), "{stdout}");
    // The test build's transport never connects, so the one-time check fails without reaching OpenAI.
    assert!(
        stdout.contains("Check failed: network error: network disabled in this test build"),
        "{stdout}"
    );
    for (what, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(!text.contains(KEY), "key in {what}: {text}");
        assert!(
            !text.contains("PTYTESTKEY"),
            "key fragment in {what}: {text}"
        );
    }
    let mut files = Vec::new();
    all_files(&s.root, &mut files);
    for f in &files {
        let bytes = fs::read(f).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("PTYTESTKEY"),
            "key in {}",
            f.display()
        );
    }
    let config = fs::read_to_string(s.config_file()).unwrap();
    assert!(config.contains("label = \"pty\""), "{config}");
    assert!(
        config.contains("allow_unencrypted_keyring = true"),
        "{config}"
    );
}

#[test]
fn key_add_gemini_with_a_gemini_key_present_refuses_before_any_prompt() {
    let s = Sandbox::new("gemini-one");
    let existing = "[[key]]\nplatform = \"gemini\"\nlabel = \"billing\"\nseries = \"00000000000000000000000000000001\"\n";
    s.write_config(existing);
    let out = s.run(&["key", "add", "gemini", "--label", "other"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("omadai key remove gemini billing"),
        "{stderr}"
    );
    for prompt in ["needs a terminal", "nothing entered", "project", "client"] {
        assert!(!stderr.contains(prompt), "{prompt}: {stderr}");
        assert!(!stdout.contains(prompt), "{prompt}: {stdout}");
    }
    assert_eq!(fs::read_to_string(s.config_file()).unwrap(), existing);
}

#[test]
fn key_add_rejects_a_bad_or_misplaced_team_id() {
    let s = Sandbox::new("team-id-usage");
    for args in [
        &["key", "add", "xai", "--team-id", "../../evil"][..],
        &["key", "add", "xai", "--team-id", ""],
        &["key", "add", "openai", "--team-id", "team-1"],
    ] {
        let out = s.run(args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!s.config_file().exists());
}

/// Runs `key add` in a pty, answering each prompt in turn, and returns whether it succeeded and its output.
#[cfg(target_os = "linux")]
fn pty_key_add(s: &Sandbox, args: &str, answers: &[(&str, &str)]) -> (bool, String) {
    let command = format!("'{BIN}' key add {args} --allow-unencrypted-keyring");
    let mut child = Command::new("script")
        .args(["-q", "-c", &command, "/dev/null"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(s.env())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("util-linux script is needed for this test");
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let output = output.clone();
        let mut stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                output.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut seen = 0;
    for (needle, answer) in answers.iter().chain(&[("Press Enter to close.", "")]) {
        let start = Instant::now();
        loop {
            let text = String::from_utf8_lossy(&output.lock().unwrap()).to_string();
            if let Some(at) = text[seen..].find(needle) {
                seen += at + needle.len();
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out waiting for {needle:?}: {text}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // rpassword prints its prompt before it turns echo off; give it time so this models a person typing.
        std::thread::sleep(Duration::from_millis(300));
        stdin.write_all(format!("{answer}\n").as_bytes()).unwrap();
    }
    drop(stdin);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let text = String::from_utf8_lossy(&output.lock().unwrap()).to_string();
    (status.success(), text)
}

#[cfg(target_os = "linux")]
#[test]
fn key_add_xai_prompts_for_the_team_id_before_the_key_and_keeps_it_in_config() {
    const KEY: &str = "fk-admin-TEAMPROMPTKEY13579BDFHJ";
    let s = Sandbox::new("team-id-prompt");
    s.unencrypted_keyring();
    let (ok, out) = pty_key_add(
        &s,
        "xai --label grok",
        &[("xAI team ID", "team-1234-abcd"), ("(input hidden): ", KEY)],
    );
    assert!(ok, "{out}");
    assert!(out.contains("Stored xAI key \"grok\""), "{out}");
    let config = fs::read_to_string(s.config_file()).unwrap();
    assert!(config.contains("[xai]"), "{config}");
    assert!(config.contains("team_id = \"team-1234-abcd\""), "{config}");
    let mut files = Vec::new();
    all_files(&s.root, &mut files);
    for f in files.iter().filter(|f| **f != s.config_file()) {
        let text = String::from_utf8_lossy(&fs::read(f).unwrap()).to_string();
        assert!(
            !text.contains("team-1234-abcd"),
            "team ID in {}",
            f.display()
        );
    }
    // A bad answer is refused before any key is asked for, and nothing is recorded.
    let t = Sandbox::new("team-id-prompt-bad");
    t.unencrypted_keyring();
    let (_, out) = pty_key_add(&t, "xai", &[("xAI team ID", "../evil")]);
    assert!(out.contains("invalid team ID"), "{out}");
    assert!(!out.contains("(input hidden)"), "{out}");
    assert!(!t.config_file().exists());
}

#[cfg(target_os = "linux")]
#[test]
fn key_add_xai_team_id_flag_skips_the_prompt() {
    const KEY: &str = "fk-admin-TEAMFLAGKEY246802468ACE";
    let s = Sandbox::new("team-id-flag");
    s.unencrypted_keyring();
    let (ok, out) = pty_key_add(
        &s,
        "xai --team-id flag-team-9",
        &[("(input hidden): ", KEY)],
    );
    assert!(ok, "{out}");
    assert!(!out.contains("xAI team ID"), "{out}");
    let config = fs::read_to_string(s.config_file()).unwrap();
    assert!(config.contains("team_id = \"flag-team-9\""), "{config}");
}

#[cfg(target_os = "linux")]
fn find_descendant(root: u32, comm: &str) -> Option<u32> {
    let parent_of = |pid: u32| -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = &stat[stat.rfind(')')? + 2..];
        after.split(' ').nth(1)?.parse().ok()
    };
    for e in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|c| c.trim() == comm)
            .unwrap_or(false)
        {
            let mut p = pid;
            while let Some(pp) = parent_of(p) {
                if pp == root {
                    return Some(pid);
                }
                if pp <= 1 {
                    break;
                }
                p = pp;
            }
        }
    }
    None
}

#[test]
fn log_lists_exports_and_clears_without_touching_home_s_mode() {
    let s = Sandbox::new("log");
    let home = s.root.join("home");
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
    let empty = s.run(&["log", "--json"]);
    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&empty.stdout), "[]\n");

    // A label with no stored credential fails the fetch, which the log records.
    s.write_config("[[key]]\nplatform = \"openai\"\nlabel = \"work\"\n");
    for _ in 0..2 {
        assert_eq!(
            s.run(&["fetch", "--month", "2026-11"]).status.code(),
            Some(1)
        );
    }
    let out = s.run(&["log", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1, "{v}");
    let e = &entries[0];
    assert_eq!(e["platform"], "openai");
    assert_eq!(e["month"], "2026-11");
    assert_eq!(e["count"], 2);
    assert_eq!(
        e["summary"],
        "A key has no stored secret · add it again in the app"
    );
    assert!(
        e["detail"]
            .as_str()
            .unwrap()
            .contains("a key label is configured but no credential is stored"),
        "{v}"
    );
    for field in ["first_at", "last_at"] {
        assert!(e[field].as_str().unwrap().ends_with('Z'), "{v}");
    }
    assert_eq!(mode(&s.state().join("errors.json")), 0o600);

    let text = String::from_utf8_lossy(&s.run(&["log"]).stdout).to_string();
    let mut lines = text.lines();
    let head = lines.next().unwrap();
    assert!(
        head.ends_with(" openai 2026-11 ×2 A key has no stored secret · add it again in the app"),
        "{text}"
    );
    assert!(lines.next().unwrap().starts_with("    "), "{text}");

    let out = s.run(&["log", "export"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let path = PathBuf::from(&printed);
    assert_eq!(path.parent(), Some(home.as_path()), "{printed}");
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.starts_with("omadai-error-log-") && name.ends_with(".txt") && name.len() == 31,
        "{name}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&home), 0o755, "HOME keeps its mode");
    // The same day's export is overwritten in place.
    assert!(s.run(&["log", "export"]).status.success());
    let exports = fs::read_dir(&home)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("omadai-error-log-")
        })
        .count();
    assert_eq!(exports, 1);

    let out = s.run(&["log", "clear"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&s.run(&["log", "--json"]).stdout),
        "[]\n"
    );
    assert_eq!(String::from_utf8_lossy(&s.run(&["log"]).stdout), "");

    for args in [
        &["log", "nope"][..],
        &["log", "export", "/tmp/x"],
        &["log", "--json", "clear"],
    ] {
        assert_eq!(s.run(args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn a_log_write_failure_is_printed_and_leaves_the_exit_code_alone() {
    let s = Sandbox::new("log-problem");
    s.write_config("[[key]]\nplatform = \"openai\"\nlabel = \"work\"\n");
    // A directory where the log lock belongs makes every log write fail.
    fs::create_dir_all(s.state().join("log.lock")).unwrap();
    let out = s.run(&["fetch", "--month", "2026-11"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("log: could not write the error log"),
        "{stderr}"
    );
    // The missing credential alone decides the code.
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

#[test]
fn a_fetch_with_an_unparsable_config_is_logged() {
    let s = Sandbox::new("log-config");
    s.write_config("[[key]\nplatform = ");
    assert_eq!(
        s.run(&["fetch", "--month", "2026-11"]).status.code(),
        Some(1)
    );
    let out = s.run(&["log", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = v.as_array().unwrap();
    assert_eq!(entries.len(), 1, "{v}");
    assert!(entries[0]["platform"].is_null(), "{v}");
    assert_eq!(entries[0]["summary"], "Fetch failed", "{v}");
}
