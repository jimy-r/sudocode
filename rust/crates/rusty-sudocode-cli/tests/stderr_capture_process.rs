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
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    struct GatedPrinter {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl rustyline::ExternalPrinter for GatedPrinter {
        fn print(&mut self, _text: String) -> rustyline::Result<()> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(())
        }
    }

    pub(super) fn exercise_rearm() {
        const BUDGET: Duration = Duration::from_secs(5);
        let (entered, printing) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let output = Arc::new(
            stderr::StderrOutput::activate(GatedPrinter {
                entered: Some(entered),
                release: released,
            })
            .expect("activate actual process capture"),
        );
        output.set_active(true).unwrap();
        output.printer().print("hold the forwarder".into()).unwrap();
        printing.recv_timeout(BUDGET).unwrap();

        let (locked, writing) = mpsc::channel();
        let (finished, written) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut stderr = std::io::stderr().lock();
            locked.send(()).unwrap();
            // Larger than the native pipe plus the Windows capture queue.
            // With the forwarder gated, this write must remain in flight.
            stderr.write_all(&vec![b'x'; 8 * 1024 * 1024]).unwrap();
            drop(stderr);
            finished.send(()).unwrap();
        });
        writing.recv_timeout(BUDGET).unwrap();
        assert!(matches!(
            written.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        // Restoring fd/handle 2 does not retarget an already-blocked write.
        // Suspension must return while that writer still owns stderr.
        output.set_active(false).unwrap();
        let (starting, started) = mpsc::channel();
        let (rearmed, ready) = mpsc::channel();
        let next = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            starting.send(()).unwrap();
            next.set_active(true).unwrap();
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
        written.recv_timeout(BUDGET).expect("drain blocked writer");
        ready.recv_timeout(BUDGET).expect("rearm after pipe drain");
        writer.join().unwrap();
        reader.join().unwrap();
        output.set_active(false).unwrap();
        drop(output);
        println!("CAPTURE_REARM_COMPLETED");
    }
}

#[tokio::test]
async fn a_full_capture_pipe_can_drain_while_the_next_prompt_rearms() {
    const CHILD: &str = "SCODE_CAPTURE_REARM_FIXTURE";
    if std::env::var_os(CHILD).is_some() {
        input::exercise_rearm();
        return;
    }
    // Keep process-global stderr replacement out of the parallel test runner.
    // Kill a deadlocked child on timeout instead of hanging the workspace job.
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_full_capture_pipe_can_drain_while_the_next_prompt_rearms",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("capture handoff must not deadlock")
    .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success() && stdout.contains("CAPTURE_REARM_COMPLETED"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
