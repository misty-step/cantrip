//! End-to-end proof of the "daemon never hangs on inject children" epic.
//!
//! The unit tests in `src/inject.rs` prove the private helpers (`run_bounded`,
//! `run_writer_backend`) time out against a wedged child. This test closes the
//! loop through the *public* `inject()` contract exactly as the oracle states:
//! with a fake `wl-copy` that sleeps far past the budget, `inject()` returns an
//! error within the configured bound instead of blocking forever.
//!
//! It lives in `tests/` as its own binary so mutating `PATH` is safe: this
//! process runs exactly one test and no in-process test shares the environment.

use cantrip::inject::{self, InjectionMode};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Write an executable `name` into `dir` that sleeps for `seconds` when run,
/// simulating a wedged compositor helper on `PATH`.
fn write_sleeping_helper(dir: &Path, name: &str, seconds: u64) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("creating fake-helper fixture dir");
    let path = dir.join(name);
    let body = format!("#!/bin/sh\nsleep {seconds}\n");
    std::fs::write(&path, body).expect("writing fake helper");
    let mut perms = std::fs::metadata(&path)
        .expect("stat fake helper")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake helper");
    path
}

#[test]
fn sleeping_wl_copy_makes_inject_fail_closed_within_budget() {
    let dir = std::env::temp_dir().join(format!(
        "cantrip-inject-e2e-fake-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before UNIX epoch")
            .as_nanos()
    ));

    // The fake is a real child that sleeps 60s — well past the 5s inject budget,
    // so a run that is not bounded would hang the suite until the 60s test
    // timeout. It sits first on PATH, shadowing any real wl-copy.
    write_sleeping_helper(&dir, "wl-copy", 60);

    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", dir.display(), old_path));

    // Latch the result and restore PATH before asserting, so even a failing
    // assert cannot leak a modified PATH into this process.
    let started = Instant::now();
    let result = inject::inject("hello world", InjectionMode::Clipboard);
    let elapsed = started.elapsed();
    std::env::set_var("PATH", &old_path);

    let _ = std::fs::remove_dir_all(&dir);

    let error = match result {
        Ok(outcome) => panic!("a sleeping wl-copy must make inject fail closed, got {outcome:?}"),
        Err(error) => error,
    };
    let error = format!("{error:#}");
    assert!(
        error.contains("wl-copy"),
        "timeout error should name the wedged backend: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "inject was not actually bounded: took {elapsed:?}"
    );
}
