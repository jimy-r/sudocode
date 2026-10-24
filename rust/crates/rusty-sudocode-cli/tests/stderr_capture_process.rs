//! Actual process stderr, OS pipe backpressure and the production forwarder.
//! The printer is gated to control scheduling; native input preservation stays
//! covered by pty_render_boundaries and pty_sync_repl_receive.

use std::time::Duration;

#[allow(dead_code)]
#[path = "../src/render"]
mod render {
    #[path = "output.rs"]
    pub(crate) mod output;
}
#[path = "../src/stderr_capture.rs"]
mod stderr_capture;

#[path = "../src/input"]
mod input {
    #[path = "stderr.rs"]
    mod stderr;

    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    struct GatedPrinter {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
        log: PathBuf,
        expired: Arc<AtomicBool>,
    }

    impl rustyline::ExternalPrinter for GatedPrinter {
        fn print(&mut self, _text: String) -> rustyline::Result<()> {
            if let Some(entered) = self.entered.take() {
                stage(&self.log, "printer gated");
                entered.send(()).unwrap();
                // Release even on a regression so a panic need not write into
                // the same full pipe. The timeout is asserted after cleanup.
                if self.release.recv_timeout(Duration::from_secs(5)).is_err() {
                    self.expired.store(true, Ordering::Release);
                    stage(&self.log, "printer gate expired");
                }
            }
            Ok(())
        }
    }

    #[inline]
    fn stage(path: &Path, text: &str) {
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(log, "{text}").unwrap();
    }

    /// A retained library handle must stay on its old pipe across dup2 or
    /// SetStdHandle. A write_all to the global stderr can instead make a short
    /// write and retry against the restored target, notably on macOS.
    #[cfg(unix)]
    #[inline]
    fn retained_stderr() -> std::fs::File {
        use std::os::fd::AsFd;
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .unwrap()
            .into()
    }

    #[cfg(windows)]
    #[inline]
    fn retained_stderr() -> std::fs::File {
        filedescriptor::FileDescriptor::dup(&std::io::stderr())
            .unwrap()
            .as_file()
            .unwrap()
    }

    pub(super) fn exercise_rearm(log: &Path, retain_handle: bool) {
        const BUDGET: Duration = Duration::from_secs(5);
        let (entered, printing) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let expired = Arc::new(AtomicBool::new(false));
        let output = Arc::new(
            stderr::StderrOutput::activate(GatedPrinter {
                entered: Some(entered),
                release: released,
                log: log.to_path_buf(),
                expired: Arc::clone(&expired),
            })
            .expect("activate actual process capture"),
        );
        stage(log, "capture created");
        output.set_active(true).unwrap();
        stage(log, "capture active");
        output.printer().print("hold the forwarder".into()).unwrap();
        printing.recv_timeout(BUDGET).unwrap();

        let mut retained = retain_handle.then(retained_stderr);
        let (locked, writing) = mpsc::channel();
        let (finished, written) = mpsc::channel();
        let writer_log = log.to_path_buf();
        let writer = std::thread::spawn(move || {
            let mut stderr = std::io::stderr().lock();
            stage(&writer_log, "writer locked stderr");
            locked.send(()).unwrap();
            // Larger than the native pipe plus the Windows capture queue.
            // With the forwarder gated, this write must remain in flight.
            let bytes = vec![b'x'; 8 * 1024 * 1024];
            let result = match retained.as_mut() {
                Some(handle) => handle.write_all(&bytes),
                None => stderr.write_all(&bytes),
            };
            drop(stderr);
            // Never panic/report an IO error while owning the global stderr
            // lock. Preserve the original result even if cleanup later stalls.
            stage(&writer_log, &format!("writer completed: {result:?}"));
            let _ = finished.send(result);
        });
        writing.recv_timeout(BUDGET).unwrap();
        let early_write = written.recv_timeout(Duration::from_millis(100));
        let was_blocked = matches!(early_write, Err(mpsc::RecvTimeoutError::Timeout));
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            // Confirm kernel backpressure, rather than inferring a full pipe
            // from a thread which might simply not have been scheduled yet.
            let stderr = std::io::stderr();
            let mut descriptors = [nix::poll::PollFd::new(
                stderr.as_fd(),
                nix::poll::PollFlags::POLLOUT,
            )];
            let deadline = std::time::Instant::now() + BUDGET;
            while nix::poll::poll(&mut descriptors, 0u16).unwrap() != 0 {
                assert!(std::time::Instant::now() < deadline, "pipe never filled");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // Suspension must not wait for a pipe writer that needs this caller
        // to release the printer gate.
        stage(log, "suspend begin");
        let suspended = output.set_active(false);
        stage(log, &format!("suspend result: {suspended:?}"));
        stage(log, "suspend completed");
        let (starting, started) = mpsc::channel();
        let (rearmed, ready) = mpsc::channel();
        let next = Arc::clone(&output);
        let reader_log = log.to_path_buf();
        let reader = std::thread::spawn(move || {
            starting.send(()).unwrap();
            stage(&reader_log, "rearm begin");
            let result = next.set_active(true);
            stage(&reader_log, &format!("rearm result: {result:?}"));
            stage(&reader_log, "rearm completed");
            let _ = rearmed.send(result);
        });
        started.recv_timeout(BUDGET).unwrap();
        let early_rearm = ready.recv_timeout(Duration::from_millis(100));
        let rearm_waited = matches!(early_rearm, Err(mpsc::RecvTimeoutError::Timeout));
        // Rearming must leave the redirect mutex available so this forwarder
        // can drain the old pipe and let the writer release stderr.
        let released = release.send(());
        stage(log, "printer released");
        let written = match early_write {
            Err(mpsc::RecvTimeoutError::Timeout) => written.recv_timeout(BUDGET),
            result => result,
        };
        let ready = match early_rearm {
            Err(mpsc::RecvTimeoutError::Timeout) => ready.recv_timeout(BUDGET),
            result => result,
        };
        writer.join().unwrap();
        reader.join().unwrap();
        output.set_active(false).unwrap();
        drop(output);
        stage(log, "capture dropped");
        assert!(!expired.load(Ordering::Acquire), "printer gate expired");
        assert!(was_blocked, "write did not reach pipe backpressure");
        if retain_handle {
            assert!(rearm_waited, "rearm bypassed the in-flight stderr writer");
        }
        suspended.expect("suspend with an in-flight writer");
        released.expect("release the gated printer");
        written
            .expect("drain blocked writer")
            .expect("stderr write");
        ready
            .expect("rearm after pipe drain")
            .expect("rearm capture");
        println!("CAPTURE_REARM_COMPLETED");
    }
}

#[tokio::test]
async fn a_full_capture_pipe_can_drain_while_the_next_prompt_rearms() {
    run_capture_case(
        "a_full_capture_pipe_can_drain_while_the_next_prompt_rearms",
        true,
    )
    .await;
}

#[tokio::test]
async fn a_global_stderr_write_can_finish_across_prompt_handoff() {
    run_capture_case(
        "a_global_stderr_write_can_finish_across_prompt_handoff",
        false,
    )
    .await;
}

async fn run_capture_case(test: &str, retain_handle: bool) {
    const CHILD: &str = "SCODE_CAPTURE_REARM_FIXTURE";
    if let Some(log) = std::env::var_os(CHILD) {
        input::exercise_rearm(std::path::Path::new(&log), retain_handle);
        return;
    }
    // Informational layouts from the actual compiler/target. These do not
    // measure retained states or attribute the rendering workload's RSS.
    println!(
        "layout bytes: CompletionState={} RetryDiagnostics={} inlineOnceLock={} boxedOnceLock={}",
        std::mem::size_of::<engine_core::subagent_completion::CompletionState>(),
        std::mem::size_of::<telemetry::RetryDiagnostics>(),
        std::mem::size_of::<std::sync::OnceLock<telemetry::RetryDiagnostics>>(),
        std::mem::size_of::<std::sync::OnceLock<Box<telemetry::RetryDiagnostics>>>(),
    );
    // Keep process-global stderr replacement out of the parallel test runner.
    // Kill a deadlocked child on timeout instead of hanging the workspace job.
    let log = tempfile::NamedTempFile::new().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, log.path())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let stages = std::fs::read_to_string(log.path()).unwrap_or_default();
    let result = result
        .unwrap_or_else(|_| panic!("capture handoff must not deadlock; stages:\n{stages}"))
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = &result.stderr[result.stderr.len().saturating_sub(8192)..];
    assert!(
        result.status.success() && stdout.contains("CAPTURE_REARM_COMPLETED"),
        "stages:\n{stages}\n{stdout}\n{}",
        String::from_utf8_lossy(stderr)
    );
}
