use std::time::Instant;

use fs2::FileExt;

use super::{lock_file, wait_for_launch};

#[test]
fn startup_waits_for_the_retiring_supervisor_to_release_ownership() {
    let root = tempfile::tempdir().unwrap();
    let retiring = lock_file(root.path(), "service.lock").unwrap();
    retiring.lock_exclusive().unwrap();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();

    let error = wait_for_launch(root.path(), &http, Instant::now())
        .err()
        .expect("a retiring supervisor still owns service.lock")
        .to_string();
    assert!(error.contains("still draining"), "{error}");
    drop(retiring);
    assert!(wait_for_launch(root.path(), &http, Instant::now()).is_ok_and(|ready| ready.is_none()));
}
