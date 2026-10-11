//! Release discovery through the real CLI and an isolated HTTP mirror.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn screen_contents(screen: &pty_expect::Screen<'_>) -> String {
    screen.contents()
}

struct Mirror {
    base: String,
    release: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mirror {
    fn new(tag: &'static str, is_released: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/release/latest", listener.local_addr().unwrap());
        let release = Arc::new(AtomicBool::new(is_released));
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let thread_release = Arc::clone(&release);
        let thread_stop = Arc::clone(&stopped);
        let thread_requests = Arc::clone(&requests);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 2048];
                let count = stream.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..count])
                    .starts_with("GET /release/latest/version.txt "));
                thread_requests.fetch_add(1, Ordering::SeqCst);
                while !thread_release.load(Ordering::SeqCst) && !thread_stop.load(Ordering::SeqCst)
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{tag}",
                    tag.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            base,
            release,
            stopped,
            requests,
            thread: Some(thread),
        }
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn startup_is_available_before_network_reply_and_next_launch_uses_cache() {
    for mode in ["off", "queue"] {
        let env = common::TestEnv::new("release-startup");
        let mirror = Mirror::new("v999.0.0", false);
        let options = [
            ("SCODE_MIRROR", mirror.base.as_str()),
            ("SCODE_CHECK_UPDATES", "1"),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", mode),
        ];
        let mut child = env.spawn_with_env(&[], &options);
        child.expect("❯").unwrap_or_else(|error| {
            panic!(
                "terminal startup: {error}\n{}",
                child.render(screen_contents)
            )
        });
        // The server withholds its reply until the real terminal is usable.
        // A synchronous startup check would block this workflow.
        mirror.release.store(true, Ordering::SeqCst);
        let cache = env.config_home().join("cache/release.json");
        let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
        while !cache.exists() {
            assert!(Instant::now() < deadline, "metadata cache was not written");
            std::thread::sleep(Duration::from_millis(20));
        }
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(record["release"]["tag"], "v999.0.0");
        assert!(record["release"]["download_base"]
            .as_str()
            .unwrap()
            .ends_with("/release/v999.0.0"));
        child.send("/exit\r").unwrap();
        assert_eq!(child.expect_eof().unwrap(), 0);

        let mut restored = env.spawn_with_env(&[], &options);
        restored.expect("Update available:").unwrap();
        restored.expect("v999.0.0").unwrap();
        restored.expect("❯").unwrap();
        restored.send("/exit\r").unwrap();
        assert_eq!(restored.expect_eof().unwrap(), 0);
        assert_eq!(mirror.requests.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn opt_out_skips_startup_network_but_explicit_check_works_on_every_platform() {
    let env = common::TestEnv::new("release-check");
    let mirror = Mirror::new("v999.0.0", true);
    let options = [
        ("SCODE_MIRROR", mirror.base.as_str()),
        ("SCODE_CHECK_UPDATES", "0"),
    ];
    let mut child = env.spawn_with_env(&[], &options);
    child.expect("❯").unwrap_or_else(|error| {
        panic!(
            "terminal startup: {error}\n{}",
            child.render(screen_contents)
        )
    });
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 0);
    let mut check = env.spawn_with_env(&["update", "--check"], &options);
    check.expect("v999.0.0").unwrap();
    check.expect("update available").unwrap();
    assert_eq!(check.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_mirror_pointer_fails_and_static_mirror_pins_its_own_version() {
    let env = common::TestEnv::new("release-invalid");
    let mirror = Mirror::new("nexusd-cohost-v999.0.0", true);
    let mut invalid = env.spawn_with_env(
        &["update", "--check"],
        &[("SCODE_MIRROR", mirror.base.as_str())],
    );
    invalid
        .expect("mirror must name a stable vX.Y.Z release")
        .unwrap_or_else(|error| {
            panic!(
                "version check: {error}\n{}",
                invalid.render(screen_contents)
            )
        });
    assert_ne!(invalid.expect_eof().unwrap(), 0);

    let pinned = mirror.base.replace("/latest", "/v999.0.0");
    let mut check =
        env.spawn_with_env(&["update", "--check"], &[("SCODE_MIRROR", pinned.as_str())]);
    check.expect("v999.0.0").unwrap();
    check.expect("update available").unwrap();
    assert_eq!(check.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 1);
}
