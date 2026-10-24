//! Real REPL, streamed provider response and shell process. Release files
//! distinguish soft steering from cancelling/restarting the tool or whole turn.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::{fs, path::PathBuf, time::Duration};

struct Release(PathBuf);
impl Drop for Release {
    fn drop(&mut self) {
        for name in ["release-tool", "release-stream", "release-provider"] {
            let _ = fs::write(self.0.join(name), "release");
        }
    }
}

fn submit(session: &mut PtySession, text: &str, key: &str) {
    session.send(&format!("\x1b[200~{text}\x1b[201~")).unwrap();
    common::expect_screen(
        session,
        |view| view.contains("Pasted") || common::screen_contains(view, text),
        common::DEFAULT_TIMEOUT,
        "draft arrived",
    );
    session.send(key).unwrap();
}

fn start(label: &str, hold_stream: bool) -> (TestEnv, PtySession, Release) {
    let env = TestEnv::new(label);
    fs::write(env.workspace_root().join("wait.sh"),
        "printf started > tool-started; while test ! -f release-tool; do sleep 0.1; done; printf 'ORIGINAL_TOOL_RESULT\\n'").unwrap();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
    let prompt = if env.is_mock() {
        let mut call = json!({"id":"steer-tool", "name":"Bash", "input":{"command":"sh wait.sh", "timeout":30000}});
        if hold_stream {
            call["stream_wait_for"] = json!(env.workspace_root().join("release-stream"));
        }
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([call])
        )
    } else {
        "Run sh wait.sh in the foreground exactly once. Do not create release-tool. After it completes, briefly report the actual result. Later input may be FYI; continue this original task unless it warrants a change.".into()
    };
    submit(&mut session, &prompt, "\r");
    common::expect_screen(
        &session,
        |_| env.workspace_root().join("tool-started").exists(),
        common::LIVE_TURN_BUDGET,
        "real shell process started",
    );
    let release = Release(env.workspace_root().to_path_buf());
    (env, session, release)
}

fn requests(env: &TestEnv) -> Vec<Value> {
    env.captured_message_bodies()
        .iter()
        .map(|text| serde_json::from_str(text).unwrap())
        .collect()
}

fn finish(session: &mut PtySession) {
    common::expect_screen(
        session,
        |view| view.contains("ctx ") && view.contains("/help"),
        common::LIVE_TURN_BUDGET,
        "turn complete",
    );
    session.send("\x15").unwrap();
    submit(session, "/exit", "\r");
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn enter_steers_without_cancelling_an_admitted_tool_or_waiting_for_response_end() {
    let (env, mut session, _release) = start("enter-steer", true);
    submit(
        &mut session,
        "FYI_STEER: keep working on the original task.",
        "\r",
    );
    common::expect_screen(
        &session,
        |view| view.contains("steering:"),
        common::DEFAULT_TIMEOUT,
        "accepted input remains visible until consumed",
    );
    fs::write(env.workspace_root().join("release-tool"), "release").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("ORIGINAL_TOOL_RESULT") && view.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "steering continued with the real tool result",
    );
    assert!(!env.workspace_root().join("release-stream").exists());
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 2, "steer continues the same tool loop");
        let next = captured[1]["messages"].to_string();
        assert!(next.contains("FYI_STEER") && next.contains("ORIGINAL_TOOL_RESULT"));
        assert!(!next.contains("Interrupted") && !next.contains("interrupted"));
    }
    finish(&mut session);
}

#[test]
fn tab_queues_for_the_next_turn_without_affecting_the_current_tool_continuation() {
    let (env, mut session, _release) = start("tab-next-turn", false);
    submit(&mut session, "NEXT_TURN_ONLY", "\t");
    common::expect_screen(
        &session,
        |view| view.contains("queued: NEXT_TURN_ONLY"),
        common::DEFAULT_TIMEOUT,
        "explicit queue",
    );
    fs::write(env.workspace_root().join("release-tool"), "release").unwrap();
    common::expect_screen_settled(
        &session,
        |view| {
            view.contains("NEXT_TURN_ONLY")
                && view.contains("ctx ")
                && view.contains("/help")
                && !view.contains("queued: NEXT_TURN_ONLY")
        },
        common::LIVE_TURN_BUDGET,
        "queued input starts a separate turn",
    );
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 3);
        assert!(!captured[1]["messages"]
            .to_string()
            .contains("NEXT_TURN_ONLY"));
        assert!(captured[2]["messages"]
            .to_string()
            .contains("NEXT_TURN_ONLY"));
    }
    finish(&mut session);
}

#[test]
fn escape_flushes_every_queued_input_without_cancellation() {
    let (env, mut session, _release) = start("escape-flush", true);
    submit(&mut session, "FLUSH_FIRST", "\t");
    submit(&mut session, "FLUSH_SECOND", "\t");
    common::expect_screen(
        &session,
        |view| view.contains("queued: FLUSH_SECOND"),
        common::DEFAULT_TIMEOUT,
        "both messages queued",
    );
    session.send("UNSENT_DRAFT").unwrap();
    common::expect_input_line(
        &session,
        "UNSENT_DRAFT",
        common::DEFAULT_TIMEOUT,
        "draft before flush",
    );
    session.send("\x1b").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("steering: FLUSH_FIRST") && view.contains("steering: FLUSH_SECOND"),
        common::DEFAULT_TIMEOUT,
        "Esc flushes the whole queue",
    );
    common::expect_input_line(
        &session,
        "UNSENT_DRAFT",
        common::DEFAULT_TIMEOUT,
        "flush preserves unsent draft",
    );
    fs::write(env.workspace_root().join("release-tool"), "release").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("ORIGINAL_TOOL_RESULT") && view.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "flush preserves current task",
    );
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 2);
        let next = captured[1]["messages"].to_string();
        assert!(next.contains("FLUSH_FIRST") && next.contains("FLUSH_SECOND"));
        assert!(!next.contains("Interrupted"));
    }
    finish(&mut session);
}

#[test]
fn ctrl_c_cancels_with_queued_input_and_busy_management_cannot_block_it() {
    let (env, mut session, _release) = start("hard-cancel-queue", false);
    submit(&mut session, "RETAIN_AFTER_CANCEL", "\t");
    submit(&mut session, "/model", "\r");
    common::expect_screen(
        &session,
        |view| view.contains("before changing session settings"),
        common::DEFAULT_TIMEOUT,
        "busy slash command returns without locking the dispatcher",
    );
    session.send("\x03").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("Cancelled") && view.contains("queued: RETAIN_AFTER_CANCEL"),
        Duration::from_secs(10),
        "hard cancellation preserves unapplied input",
    );
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            1,
            "cancel does not restart queued work"
        );
    }
    submit(&mut session, "/exit", "\r");
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn enter_after_ctrl_c_resumes_retained_input_after_the_cancelled_worker_settles() {
    let (env, mut session, _release) = start("resume-after-cancel", false);
    submit(&mut session, "RETAINED_BEFORE_CANCEL", "\t");
    common::expect_screen(
        &session,
        |view| view.contains("queued: RETAINED_BEFORE_CANCEL"),
        common::DEFAULT_TIMEOUT,
        "explicit queue before cancellation",
    );
    // Do not wait for cancellation to finish: this is the actual input race.
    session.send("\x03").unwrap();
    submit(&mut session, "NEW_INPUT_AFTER_CANCEL", "\r");
    common::expect_screen_settled(
        &session,
        |view| {
            view.contains("NEW_INPUT_AFTER_CANCEL")
                && view.contains("ctx ")
                && view.contains("/help")
                && !view.contains("queued:")
        },
        common::LIVE_TURN_BUDGET,
        "new Enter explicitly resumes the retained queue",
    );
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 2);
        let context = captured[1]["messages"].to_string();
        assert!(
            context.contains("RETAINED_BEFORE_CANCEL")
                && context.contains("NEW_INPUT_AFTER_CANCEL")
        );
    }
    finish(&mut session);
}

fn inject_peer_input(env: &TestEnv, delivery: engine_events::InputDelivery, body: &str) {
    use runtime::agent_mailbox::{self, MailboxEnvelope};
    use runtime::mailbox::{local_pair_root_in, Mailbox};
    let pair = local_pair_root_in(env.config_home());
    let receiver = fs::read_dir(pair.join("agents"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("conversations").is_dir())
        .expect("running receiver announced its mailbox");
    Mailbox::workspace_local(&pair, "fixture-peer".into())
        .send(MailboxEnvelope {
            delivery,
            from: "fixture-peer".into(),
            to: receiver.file_name().unwrap().to_string_lossy().into_owned(),
            body: body.into(),
            summary: Some("auxiliary peer information".into()),
            timestamp: 0,
            color: None,
            kind: agent_mailbox::kinds::MESSAGE.into(),
            request_id: None,
        })
        .unwrap();
}

fn peer_input(delivery: engine_events::InputDelivery) {
    let (env, mut session, _release) = start("peer-steering", true);
    inject_peer_input(
        &env,
        delivery,
        "PEER_FYI: this is only auxiliary information; continue the human's original task.",
    );
    let label = if delivery == engine_events::InputDelivery::Queue {
        "queued:"
    } else {
        "steering:"
    };
    common::expect_screen(
        &session,
        |view| {
            view.lines()
                .any(|line| line.contains(label) && line.contains("fixture-peer"))
        },
        common::DEFAULT_TIMEOUT,
        "peer delivery intent projected by the shared input queue",
    );
    if delivery == engine_events::InputDelivery::Queue {
        session.send("\x1b").unwrap();
        common::expect_screen(
            &session,
            |view| view.contains("steering:") && view.contains("fixture-peer"),
            common::DEFAULT_TIMEOUT,
            "human Esc flushes queued peer input too",
        );
    }
    fs::write(env.workspace_root().join("release-tool"), "release").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("Message from fixture-peer") && view.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "peer frame committed during the original task",
    );
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 2);
        let context = captured[1]["messages"].to_string();
        assert!(context.contains("mailbox-message") && context.contains("PEER_FYI"));
        assert!(context.contains("ORIGINAL_TOOL_RESULT") && !context.contains("Interrupted"));
    }
    finish(&mut session);
}

#[test]
fn peer_steering_preserves_source_and_shares_the_human_delivery_boundary() {
    peer_input(engine_events::InputDelivery::Steer);
}

#[test]
fn escape_flushes_explicitly_queued_peer_input_without_interrupting_the_task() {
    peer_input(engine_events::InputDelivery::Queue);
}

#[test]
fn core_send_delivery_reaches_another_real_repl_without_losing_its_intent() {
    use runtime::mailbox::local_pair_root_in;
    let (env, mut receiver, _release) = start("core-send-delivery", true);
    let agents = local_pair_root_in(env.config_home()).join("agents");
    let target = fs::read_dir(agents)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("conversations").is_dir())
        .unwrap();
    let target = target.file_name().unwrap().to_string_lossy().into_owned();
    let sender_config = env.workspace_root().join("sender-settings");
    fs::create_dir_all(&sender_config).unwrap();
    fs::write(
        sender_config.join("settings.json"),
        r#"{"agentName":"fixture-sender"}"#,
    )
    .unwrap();
    let mut sender = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SCODE_PROJECT_CONFIG_DIR", sender_config.to_str().unwrap()),
        ],
    );
    sender.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&sender, Duration::from_secs(30), "sender prompt");
    let body =
        "PEER_CORE_SEND_BODY: auxiliary FYI only; continue the original task and do not reply.";
    let calls = json!([{"id":"core-delivery", "name":"send", "input":{
        "to":target, "message":body, "summary":"queued auxiliary information", "delivery":"queue"
    }}]);
    let prompt = if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        format!("Call send exactly once to {target}, with message {body:?}, summary queued auxiliary information and delivery queue. Do not use other tools. Then briefly acknowledge completion.")
    };
    submit(&mut sender, &prompt, "\r");
    common::expect_screen(
        &receiver,
        |view| {
            view.lines()
                .any(|line| line.contains("queued:") && line.contains("PEER_CORE_SEND_BODY"))
        },
        common::LIVE_TURN_BUDGET,
        "core send's queue intent survives transport and receipt",
    );
    finish(&mut sender);
    receiver.send("\x1b").unwrap();
    common::expect_screen(
        &receiver,
        |view| view.contains("steering:") && view.contains("PEER_CORE_SEND_BODY"),
        common::DEFAULT_TIMEOUT,
        "recipient human flushes the queued core-tool message",
    );
    fs::write(env.workspace_root().join("release-tool"), "release").unwrap();
    common::expect_screen_settled(
        &receiver,
        |view| {
            view.contains("ORIGINAL_TOOL_RESULT") && view.contains("ctx ") && view.contains("/help")
        },
        common::LIVE_TURN_BUDGET,
        "original task finishes after receiving peer input",
    );
    if env.is_mock() {
        let captured = requests(&env);
        let delivered: Vec<_> = captured
            .iter()
            .filter(|request| {
                let context = request["messages"].to_string();
                context.contains("ORIGINAL_TOOL_RESULT") && context.contains("PEER_CORE_SEND_BODY")
            })
            .collect();
        assert_eq!(
            delivered.len(),
            1,
            "recipient continuation receives the peer message once"
        );
        let context = delivered[0]["messages"].to_string();
        assert!(context.contains("mailbox-message") && !context.contains("Interrupted"));
    }
    finish(&mut receiver);
}

#[test]
fn streaming_text_is_a_safe_steering_boundary_without_waiting_for_block_stop() {
    // A transport-controlled text block never closes. This tests partial-text
    // replay separately from the live real-tool workflows above.
    let env = TestEnv::new_mock("text-steering-boundary");
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
    let ready = env.workspace_root().join("text-ready");
    let release = env.workspace_root().join("release-stream");
    let _release = Release(env.workspace_root().to_path_buf());
    let prompt = common::terminal_host::prose_prompt(
        &env,
        "STREAM_TEXT_BEFORE_STEER.\n\n",
        json!({"ready":ready, "release":release}),
    );
    submit(&mut session, &prompt, "\r");
    common::expect_screen(
        &session,
        |_| ready.exists(),
        common::DEFAULT_TIMEOUT,
        "text streamed but block is open",
    );
    submit(
        &mut session,
        "FYI_TEXT_STEER PARITY_SCENARIO:single_turn_text",
        "\r",
    );
    common::expect_screen(
        &session,
        |view| view.contains("The answer is 4") && view.contains("ctx "),
        common::DEFAULT_TIMEOUT,
        "steering preempts an open text block",
    );
    let captured = requests(&env);
    assert_eq!(captured.len(), 2);
    let context = captured[1]["messages"].to_string();
    assert!(context.contains("STREAM_TEXT_BEFORE_STEER") && context.contains("FYI_TEXT_STEER"));
    assert!(!context.contains("Interrupted"));
    assert!(!release.exists());
    finish(&mut session);
}

#[cfg(unix)]
fn parallel_pipes(env: &TestEnv) {
    use nix::{sys::stat::Mode, unistd::mkfifo};
    for name in ["first.pipe", "second.pipe"] {
        mkfifo(
            &env.workspace_root().join(name),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .unwrap();
    }
}

#[cfg(unix)]
fn fifo_writer(env: &TestEnv, name: &str) -> std::fs::File {
    use nix::libc;
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, time::Instant};
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(env.workspace_root().join(name))
        {
            Ok(writer) => return writer,
            Err(error)
                if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("parallel reader {name} did not start: {error}"),
        }
    }
}

#[cfg(unix)]
fn ordered_parallel_rows(view: &str, label: &str) -> bool {
    match (
        view.rfind("Bash(head -n 1 first.pipe)"),
        view.rfind(label),
        view.rfind("Bash(head -n 1 second.pipe)"),
    ) {
        (Some(first), Some(message), Some(second)) => first < message && message < second,
        _ => false,
    }
}

#[cfg(unix)]
#[test]
fn steering_settles_every_parallel_tool_before_inserting_input() {
    use std::io::Write;
    let env = TestEnv::new("parallel-steering-boundary");
    parallel_pipes(&env);
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_MAX_TOOL_USE_CONCURRENCY", "2"),
        ],
    );
    session.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "parallel prompt");
    let _release = Release(env.workspace_root().to_path_buf());
    let mut calls = json!([
        {"id":"steer-first", "name":"Bash", "input":{"command":"head -n 1 first.pipe"}},
        {"id":"steer-second", "name":"Bash", "input":{"command":"head -n 1 second.pipe"}},
    ]);
    if env.is_mock() {
        calls[0]["stream_wait_for"] = json!(env.workspace_root().join("release-provider"));
    }
    let prompt = if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        format!("Issue these two Bash calls together in one message, exactly once, in the foreground. Do not write to the named pipes; the test runner supplies their contents. After both finish, report the actual results: {calls}")
    };
    submit(&mut session, &prompt, "\r");
    let mut first = fifo_writer(&env, "first.pipe");
    // Live providers emit the pair together. The controlled transport additionally
    // proves that the input keeps its position when a later tool card appears.
    submit(
        &mut session,
        "PARALLEL_STEER_FYI: continue the original task.",
        "\t",
    );
    common::expect_screen(
        &session,
        |view| view.contains("queued: PARALLEL_STEER_FYI"),
        common::DEFAULT_TIMEOUT,
        "queue input before admitting the second streamed call",
    );
    fs::write(env.workspace_root().join("release-provider"), "release").unwrap();
    let mut second = fifo_writer(&env, "second.pipe");
    if env.is_mock() {
        common::expect_screen(
            &session,
            |view| ordered_parallel_rows(view, "queued: PARALLEL_STEER_FYI"),
            common::DEFAULT_TIMEOUT,
            "pending rows reflect tool/input arrival order",
        );
    }
    session.send("\x1b").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("steering: PARALLEL_STEER_FYI"),
        common::DEFAULT_TIMEOUT,
        "flush while both real processes are running",
    );
    if env.is_mock() {
        common::expect_screen(
            &session,
            |view| ordered_parallel_rows(view, "steering: PARALLEL_STEER_FYI"),
            common::DEFAULT_TIMEOUT,
            "queue-to-steering transfer updates the same row in place",
        );
    }
    writeln!(first, "FIRST_REAL_RESULT").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("FIRST_REAL_RESULT"),
        common::DEFAULT_TIMEOUT,
        "first tool finishes independently",
    );
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            1,
            "remaining admitted tool must settle first"
        );
    }
    writeln!(second, "SECOND_REAL_RESULT").unwrap();
    common::expect_screen_settled(
        &session,
        |view| {
            view.contains("SECOND_REAL_RESULT") && view.contains("ctx ") && view.contains("/help")
        },
        common::LIVE_TURN_BUDGET,
        "parallel exchange completed before steering continuation",
    );
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 2);
        let context = captured[1]["messages"].to_string();
        for text in [
            "FIRST_REAL_RESULT",
            "SECOND_REAL_RESULT",
            "PARALLEL_STEER_FYI",
        ] {
            assert!(context.contains(text));
        }
        assert!(!context.contains("Interrupted"));
    }
    finish(&mut session);
}

// Exercise signed consecutive thinking and unadmitted tool input over HTTP,
// while the real REPL and executor remain unchanged.
fn pre_admission_boundary(pause_at: &str) {
    let env = TestEnv::new_mock(&format!("pre-admission-{pause_at}"));
    let root = env.workspace_root();
    let gate = root.join("release-stream");
    let ready = root.join("release-stream.ready");
    let _release = Release(root.to_path_buf());
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
    let blocks = json!([
        {"thinking":"FIRST_COMPLETE_THOUGHT", "signature":"signed-first"},
        {"thinking":"SECOND_COMPLETE_THOUGHT", "signature":"signed-second"}
    ]);
    let call = json!({"id":"must-not-admit", "name":"Bash", "input":{"command":"printf wrongly-admitted > unexpected-effect"},
        "stream_wait_for":gate, "stream_pause_at":pause_at, "stream_thinking":blocks});
    submit(
        &mut session,
        &format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([call])
        ),
        "\r",
    );
    common::expect_screen(
        &session,
        |_| ready.exists(),
        common::DEFAULT_TIMEOUT,
        "provider boundary is held open",
    );
    // A server-side write is not client consumption. Reasoning activity
    // confirms the client has entered thinking; its text may be folded in UI.
    common::expect_screen(
        &session,
        |view| view.contains("Reasoning"),
        common::DEFAULT_TIMEOUT,
        "client entered thinking before steering",
    );
    submit(
        &mut session,
        "FYI_BOUNDARY_STEER PARITY_SCENARIO:single_turn_text",
        "\r",
    );
    if pause_at == "thinking" {
        // Queued input must not split a consecutive thinking sequence.
        common::expect_screen(
            &session,
            |view| view.contains("steering:"),
            common::DEFAULT_TIMEOUT,
            "steer waits for the complete thinking sequence",
        );
        assert_eq!(requests(&env).len(), 1);
        fs::write(&gate, "release").unwrap();
    }
    common::expect_screen(
        &session,
        |view| view.contains("The answer is 4") && view.contains("ctx "),
        common::DEFAULT_TIMEOUT,
        "steer preempts before unadmitted tool execution",
    );
    assert!(!root.join("unexpected-effect").exists());
    let captured = requests(&env);
    assert_eq!(captured.len(), 2);
    let messages = captured[1]["messages"].as_array().unwrap();
    let thinking: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|block| block["type"] == "thinking")
        .map(|block| json!({"thinking":block["thinking"], "signature":block["signature"]}))
        .collect();
    assert_eq!(
        thinking,
        blocks.as_array().unwrap().clone(),
        "complete signed thinking sequence replays unchanged"
    );
    let content = captured[1]["messages"].to_string();
    assert!(!content.contains("\"tool_use\"") && !content.contains("Interrupted"));
    assert!(content.contains("FYI_BOUNDARY_STEER"));
    if pause_at == "arguments" {
        assert!(!gate.exists(), "partial tool input never needs to finish");
    }
    finish(&mut session);
}

#[test]
fn steering_preserves_the_complete_consecutive_thinking_sequence() {
    pre_admission_boundary("thinking");
}

#[test]
fn steering_drops_unadmitted_tool_arguments_without_external_effects() {
    pre_admission_boundary("arguments");
}

#[test]
fn escape_flush_preserves_the_question_and_its_answer_owner() {
    let env = TestEnv::new("flush-awaiting-answer");
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"question-owner", "name":"AskUserQuestion", "input":{"question":"FLUSH_KEEPS_QUESTION"}}])
        )
    } else {
        "Call AskUserQuestion exactly once with the question FLUSH_KEEPS_QUESTION. Wait for my answer, then acknowledge it without other tools. A peer may supply auxiliary FYI; it must not answer the question.".into()
    };
    submit(&mut session, &prompt, "\r");
    common::expect_screen(
        &session,
        |view| view.contains("1/1  FLUSH_KEEPS_QUESTION"),
        common::LIVE_TURN_BUDGET,
        "question owns input",
    );
    inject_peer_input(
        &env,
        engine_events::InputDelivery::Queue,
        "PEER_QUESTION_FYI: auxiliary information only; no reply needed.",
    );
    common::expect_screen(
        &session,
        |view| view.contains("queued:") && view.contains("1/1  FLUSH_KEEPS_QUESTION"),
        common::DEFAULT_TIMEOUT,
        "peer queued beside the unanswered question",
    );
    session.send("\x1b").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("steering:") && view.contains("1/1  FLUSH_KEEPS_QUESTION"),
        common::DEFAULT_TIMEOUT,
        "flush leaves the active question visible",
    );
    session.send("ANSWER_FROM_HUMAN").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("ANSWER_FROM_HUMAN"),
        common::DEFAULT_TIMEOUT,
        "human answer remains with its question",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("ctx ") && !view.contains("steering:"),
        common::LIVE_TURN_BUDGET,
        "answer and auxiliary input complete the original task",
    );
    if env.is_mock() {
        let captured = requests(&env);
        assert_eq!(captured.len(), 2);
        let context = captured[1]["messages"].to_string();
        assert!(context.contains("ANSWER_FROM_HUMAN") && context.contains("PEER_QUESTION_FYI"));
        assert!(!context.contains("Interrupted") && !context.contains("interrupted"));
    }
    finish(&mut session);
}

#[test]
fn ctrl_c_from_the_task_browser_cancels_even_with_queued_input() {
    let (env, mut session, _release) = start("task-browser-cancel", true);
    submit(&mut session, "RETAINED_FROM_TASK_BROWSER", "\t");
    common::expect_screen(
        &session,
        |view| view.contains("queued: RETAINED_FROM_TASK_BROWSER"),
        common::DEFAULT_TIMEOUT,
        "input queued before task inspection",
    );
    submit(&mut session, "/tasks", "\r");
    common::expect_screen(
        &session,
        |view| view.contains("Background tasks") && view.contains("No background tasks"),
        common::DEFAULT_TIMEOUT,
        "task browser owns the keyboard",
    );
    session.send("\x03").unwrap();
    common::expect_screen(
        &session,
        |view| {
            view.to_lowercase().contains("cancelled")
                && view.contains("queued: RETAINED_FROM_TASK_BROWSER")
                && !view.contains("No background tasks")
        },
        common::DEFAULT_TIMEOUT,
        "Ctrl+C closes task inspection and cancels work without flushing the queue",
    );
    if env.is_mock() {
        assert_eq!(
            requests(&env).len(),
            1,
            "cancelled work must not restart the retained queue"
        );
    }
    finish(&mut session);
}
