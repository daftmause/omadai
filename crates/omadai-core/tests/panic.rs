//! A panic while a credential is in scope must not print it.

use omadai_core::Secret;
use omadai_core::http::{Request, default_transport};
use std::process::Command;

const KEY: &str = "fk-admin-PANICTESTKEY1357924680";

/// Runs only as the child of `panic_output_has_no_key`, selected by the env var it sets.
#[test]
fn panicking_child() {
    if std::env::var_os("OMADAI_PANIC_CHILD").is_none() {
        return;
    }
    let secret = Secret::new(KEY.to_string());
    let request = Request::get("http://api.openai.com/v1/organization/costs")
        .secret_header("Authorization", Secret::with_prefix("Bearer ", &secret));
    let result = default_transport().send(&request, &secret);
    let err = result.expect_err("http is refused");
    panic!("provider call failed: {err}; request {request:?}; secret {secret:?}");
}

#[test]
fn panic_output_has_no_key() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "panicking_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("OMADAI_PANIC_CHILD", "1")
        .env("RUST_BACKTRACE", "full")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the child must have panicked");
    assert!(
        stderr.contains("provider call failed") || stdout.contains("provider call failed"),
        "panic message missing:\n{stderr}"
    );
    for text in [&stdout, &stderr] {
        assert!(!text.contains(KEY), "key printed by panic:\n{text}");
        assert!(
            !text.contains("PANICTESTKEY"),
            "key fragment printed by panic:\n{text}"
        );
    }
}
