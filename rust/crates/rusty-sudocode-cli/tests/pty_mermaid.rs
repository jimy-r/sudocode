//! Actual streamed rendering, input ownership and replay in both terminal UIs.
mod common;

use common::TestEnv;
use std::time::Duration;

const DOCUMENT: &str =
    "DiagramBefore.\n\n```mermaid\nflowchart TD\nA[请求] --> B[Reply]\n```\n\nDiagramDone.";

fn assert_diagram(screen: &str) {
    assert!(
        screen.contains("请求") && screen.contains("Reply") && screen.contains('─'),
        "{screen}"
    );
    assert!(
        !screen.contains("flowchart TD") && !screen.contains("source shown"),
        "{screen}"
    );
}

#[test]
fn mermaid_stream_and_replay_preserve_input_and_original_markdown() {
    for mode in ["off", "queue"] {
        eprintln!("Mermaid native fixture mode={mode}");
        let env = TestEnv::new_mock("mermaid-stream-replay");
        let prompt = common::terminal_host::prose_prompt(&env, DOCUMENT, serde_json::json!({}));
        let mut session = env.spawn_with_env(
            &["--permission-mode", "read-only"],
            &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode), ("NO_COLOR", "1")],
        );
        session.resize(60, 100).unwrap();
        common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
        let initial_status = common::turn_status_marker(&session);
        session.send(&prompt).unwrap();
        let complete_prompt = prompt.split_whitespace().collect::<String>();
        common::expect_screen(
            &session,
            |screen| {
                screen
                    .split_whitespace()
                    .collect::<String>()
                    .contains(&complete_prompt)
            },
            common::DEFAULT_TIMEOUT,
            "complete prompt across wrapped input rows",
        );
        session.send("\r").unwrap();
        let screen = common::expect_screen_settled(
            &session,
            |s| {
                s.contains("请求")
                    && s.contains("Reply")
                    && env.workspace_root().join("prose-ready").exists()
            },
            common::DEFAULT_TIMEOUT,
            "streamed Mermaid before EOF",
        );
        assert_diagram(&screen);
        if mode == "queue" {
            session.send("draftkeep").unwrap();
            common::expect_input_line(
                &session,
                "draftkeep",
                common::DEFAULT_TIMEOUT,
                "draft during stream",
            );
        }
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        if mode == "off" {
            common::expect_turn_complete_after(
                &session,
                &initial_status,
                common::DEFAULT_TIMEOUT,
                "classic turn completed",
            );
            session.send("draftkeep").unwrap();
        }
        common::expect_screen_settled(
            &session,
            |s| {
                let status = common::turn_status_identity(s);
                !status.is_empty()
                    && status != initial_status
                    && s.contains("DiagramDone.")
                    && common::input_line_of(s) == "draftkeep"
            },
            common::DEFAULT_TIMEOUT,
            "draft after handoff",
        );
        session.send("\x15/exit").unwrap();
        common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "exit input");
        session.send("\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
        let requests = env.captured_message_bodies();
        let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        let instructions = request["system"].to_string();
        let received_full_prompt = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .filter_map(|block| block["text"].as_str())
            .any(|text| text.contains(&prompt));
        assert!(
            received_full_prompt,
            "the exact complete prompt must reach the model"
        );
        assert!(
            instructions.contains("# Diagrams") && instructions.contains("fenced `mermaid` block"),
            "diagram guidance must reach the actual model request: {instructions}"
        );
        let transcript =
            common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
                .expect("persisted transcript");
        let saved = runtime::Session::load_from_path(&transcript).unwrap();
        assert!(
            saved.messages().iter().any(|message| {
                message.blocks.iter().any(|block|
            matches!(block, runtime::ContentBlock::Text { text } if text == DOCUMENT)
        )
            }),
            "original Mermaid Markdown must survive display unchanged"
        );

        let mut replay = env.spawn_with_env(
            &["--resume", "latest", "--permission-mode", "read-only"],
            &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode), ("NO_COLOR", "1")],
        );
        replay.resize(60, 100).unwrap();
        let screen = common::expect_screen_settled(
            &replay,
            |s| s.contains("DiagramDone."),
            common::DEFAULT_TIMEOUT,
            "replayed Mermaid",
        );
        assert_diagram(&screen);
        common::expect_input_line_cleared(&replay, common::DEFAULT_TIMEOUT, "replay ready");
        replay.send("/exit").unwrap();
        common::expect_input_line(&replay, "/exit", common::DEFAULT_TIMEOUT, "replay exit");
        replay.send("\r").unwrap();
        assert_eq!(replay.expect_eof().unwrap(), 0);
    }
}
