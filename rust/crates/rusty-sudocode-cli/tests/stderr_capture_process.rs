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
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    struct GatedPrinter {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
        log: PathBuf,
    }

    impl rustyline::ExternalPrinter for GatedPrinter {
        fn print(&mut self, _text: String) -> rustyline::Result<()> {
            if let Some(entered) = self.entered.take() {
                stage(&self.log, "printer gated");
                entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
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

    pub(super) fn exercise_rearm(log: &Path) {
        const BUDGET: Duration = Duration::from_secs(5);
        let (entered, printing) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let output = Arc::new(
            stderr::StderrOutput::activate(GatedPrinter {
                entered: Some(entered),
                release: released,
                log: log.to_path_buf(),
            })
            .expect("activate actual process capture"),
        );
        stage(log, "capture created");
        output.set_active(true).unwrap();
        stage(log, "capture active");
        output.printer().print("hold the forwarder".into()).unwrap();
        printing.recv_timeout(BUDGET).unwrap();

        let (locked, writing) = mpsc::channel();
        let (finished, written) = mpsc::channel();
        let writer_log = log.to_path_buf();
        let writer = std::thread::spawn(move || {
            let mut stderr = std::io::stderr().lock();
            stage(&writer_log, "writer locked stderr");
            locked.send(()).unwrap();
            // Larger than the native pipe plus the Windows capture queue.
            // With the forwarder gated, this write must remain in flight.
            stderr.write_all(&vec![b'x'; 8 * 1024 * 1024]).unwrap();
            drop(stderr);
            stage(&writer_log, "writer completed");
            finished.send(()).unwrap();
        });
        writing.recv_timeout(BUDGET).unwrap();
        assert!(matches!(
            written.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
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

        // Restoring fd/handle 2 does not retarget an already-blocked write.
        // Suspension must return while that writer still owns stderr.
        stage(log, "suspend begin");
        output.set_active(false).unwrap();
        stage(log, "suspend completed");
        let (starting, started) = mpsc::channel();
        let (rearmed, ready) = mpsc::channel();
        let next = Arc::clone(&output);
        let reader_log = log.to_path_buf();
        let reader = std::thread::spawn(move || {
            starting.send(()).unwrap();
            stage(&reader_log, "rearm begin");
            next.set_active(true).unwrap();
            stage(&reader_log, "rearm completed");
            rearmed.send(()).unwrap();
        });
        started.recv_timeout(BUDGET).unwrap();
        assert!(matches!(
            ready.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        // Rearming must leave the redirect mutex available so this forwarder
        // can drain the old pipe and let the writer release stderr.
        release.send(()).unwrap();
        stage(log, "printer released");
        written.recv_timeout(BUDGET).expect("drain blocked writer");
        ready.recv_timeout(BUDGET).expect("rearm after pipe drain");
        writer.join().unwrap();
        reader.join().unwrap();
        output.set_active(false).unwrap();
        drop(output);
        stage(log, "capture dropped");
        println!("CAPTURE_REARM_COMPLETED");
    }
}

#[tokio::test]
async fn a_full_capture_pipe_can_drain_while_the_next_prompt_rearms() {
    const CHILD: &str = "SCODE_CAPTURE_REARM_FIXTURE";
    if let Some(log) = std::env::var_os(CHILD) {
        input::exercise_rearm(std::path::Path::new(&log));
        return;
    }
    // Keep process-global stderr replacement out of the parallel test runner.
    // Kill a deadlocked child on timeout instead of hanging the workspace job.
    let log = tempfile::NamedTempFile::new().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_full_capture_pipe_can_drain_while_the_next_prompt_rearms",
                "--nocapture",
            ])
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
    assert!(
        result.status.success() && stdout.contains("CAPTURE_REARM_COMPLETED"),
        "stages:\n{stages}\n{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
