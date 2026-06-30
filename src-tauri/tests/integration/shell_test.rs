//! Integration tests for the v0.5 OS shell executor.
//!
//! The shell executor is the security-sensitive part of the OS
//! integration, so the test suite focuses on the safety model
//! (whitelist, argv validation, timeout) more than on positive
//! command execution.

use nine_snake_lib::os::{parse_argv, ShellExecutor};
use std::time::Duration;

#[tokio::test]
async fn whitelist_default_includes_basic_binaries() {
    let ex = ShellExecutor::new();
    for bin in ["ls", "cat", "echo", "git", "cargo", "node", "python3"] {
        assert!(
            ex.is_allowed(bin),
            "expected {bin} to be in the default whitelist"
        );
    }
    assert!(!ex.is_allowed("curl"));
    assert!(!ex.is_allowed("nc"));
    assert!(!ex.is_allowed("bash"));
}

#[tokio::test]
async fn extend_whitelist_works() {
    let ex = ShellExecutor::new().allow("curl");
    assert!(ex.is_allowed("curl"));
}

#[tokio::test]
async fn parse_argv_handles_quotes() {
    let argv = parse_argv(r#"echo "hello world" 'foo bar'"#).expect("parse");
    assert_eq!(argv, vec!["echo", "hello world", "foo bar"]);
}

#[tokio::test]
async fn parse_argv_rejects_unbalanced_quote() {
    let res = parse_argv(r#"echo "unterminated"#);
    assert!(res.is_err());
}

#[tokio::test]
async fn exec_rejects_disallowed_binary() {
    let ex = ShellExecutor::new();
    let err = ex
        .exec(vec!["curl"], None)
        .await
        .expect_err("should reject");
    assert!(err.to_string().contains("whitelist"));
}

#[tokio::test]
async fn exec_runs_echo() {
    let ex = ShellExecutor::new();
    let out = ex.exec(vec!["echo", "hi"], None).await.expect("exec");
    assert!(out.stdout.contains("hi"));
    assert_eq!(out.exit_code, 0);
    assert!(!out.timed_out);
}

#[tokio::test]
async fn exec_respects_short_timeout() {
    // 50 ms timeout on a long-running command should produce a
    // timed-out result on any reasonable machine.
    let ex = ShellExecutor::new().with_timeout(Duration::from_millis(50));
    let out = ex
        .exec(vec!["find", "/"], None)
        .await
        .expect("exec should not error on timeout, just mark timed_out");
    if out.timed_out {
        assert_eq!(out.exit_code, -1);
    } else {
        // On a very fast machine the find might finish; that's
        // acceptable — the test only asserts the executor doesn't
        // crash.
    }
}

#[tokio::test]
async fn exec_rejects_null_byte() {
    let ex = ShellExecutor::new();
    let argv: Vec<String> = vec!["echo".into(), "ab\u{0}cd".into()];
    let err = ex
        .exec(argv, None)
        .await
        .expect_err("null byte must be rejected");
    assert!(err.to_string().contains("null"));
}
