//! Exercise the public delivery deadline without a desktop or real clipboard.
//! This binary has one test, so its PATH override cannot affect another test.

use cantrip::inject::{self, DeliveryGuard, InjectionFailureKind, InjectionMode};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn nonreading_clipboard_helper_is_bounded_and_reaped() {
    let directory = std::env::temp_dir().join(format!(
        "cantrip-inject-timeout-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let helper = directory.join("wl-copy");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$CANTRIP_TEST_HELPER_PID\"\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let search_path = std::env::join_paths(
        std::iter::once(directory.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    std::env::set_var("PATH", search_path);
    std::env::set_var("CANTRIP_TEST_HELPER_PID", directory.join("pid"));

    // Exceed pipe capacity: the deadline must cover writing, not only child exit.
    let payload = "synthetic-private-dictation".repeat(65_536);
    let started = Instant::now();
    let result = inject::inject(
        &payload,
        InjectionMode::Clipboard,
        &DeliveryGuard::capture(),
        &AtomicBool::new(false),
    );
    let elapsed = started.elapsed();
    std::env::set_var("PATH", original_path);
    std::env::remove_var("CANTRIP_TEST_HELPER_PID");

    let pid = std::fs::read_to_string(directory.join("pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    std::fs::remove_dir_all(&directory).unwrap();

    let error = result.unwrap_err();
    assert_eq!(error.kind, InjectionFailureKind::Failed);
    assert!(!error.to_string().contains("synthetic-private-dictation"));
    assert!(
        elapsed < Duration::from_secs(8),
        "delivery took {elapsed:?}"
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
