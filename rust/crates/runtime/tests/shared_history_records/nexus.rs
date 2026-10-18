//! The same context boundaries over real mTLS clients and daemon restart.
use std::path::Path;
use std::sync::Arc;

use runtime::fs_backend::{FsBackend, NexusVfsFsBackend, RecordLogMode};
use runtime::history::{HistoryLog, HistoryRecord};
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};

use super::{append, assistant, commit, finish_turn, text, wait_for, Fixture, ToolChild};

fn live_backend() -> NexusVfsFsBackend {
    let credential = runtime::nexus_mailbox::AgentCredential::load(
        &std::env::var("NEXUS_A2A_TEST_CERT_DIR").expect("test credential directory"),
    )
    .unwrap();
    let client = runtime::nexus_mailbox::Config {
        endpoint: std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("test endpoint"),
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .unwrap();
    NexusVfsFsBackend::new(
        Arc::try_unwrap(client).unwrap_or_else(|_| panic!("unshared test connection")),
        std::env::var("NEXUS_API_KEY").unwrap_or_default(),
    )
}

fn seed(log: &mut HistoryLog) -> Session {
    let mut prefix = Session::new();
    prefix.push_user_text("NEXUS_COMMON_PREFIX").unwrap();
    prefix.push_message(text("NEXUS_COMMON_ANSWER")).unwrap();
    commit(log, &HistoryRecord::initialize(&prefix).unwrap());
    prefix
}

#[test]
#[ignore = "real mTLS daemon; e2e/nexus-a2a/run.sh supplies the fixture"]
fn live_shared_history_preserves_contexts_while_sibling_tool_runs() {
    let fixture = Fixture::new();
    let fs = live_backend();
    let directory = format!("/agents/live-probe/history-{}", uuid::Uuid::new_v4());
    fs.create_dir_all(&directory).unwrap();
    let path = format!("{directory}/events");
    let mut a = HistoryLog::new(fs.record_log(&path, RecordLogMode::CreateNew).unwrap());
    let prefix = seed(&mut a);
    let branch_a = HistoryRecord::branch_latest();
    commit(&mut a, &branch_a);
    let context_a = branch_a.context_id();
    append(
        &mut a,
        context_a,
        &ConversationMessage::user_text("NEXUS_A_PRIVATE"),
    );
    let assistant_id = append(
        &mut a,
        context_a,
        &assistant(vec![ContentBlock::ToolUse {
            id: "nexus-call-a".into(),
            name: "fixture_tool".into(),
            input: "{}".into(),
            thought_signature: None,
        }]),
    );
    let mut tool = ToolChild::spawn(&fixture.0);
    wait_for(&fixture.0.join("tool-started"));
    let mut b = HistoryLog::new(
        live_backend()
            .record_log(&path, RecordLogMode::Existing)
            .unwrap(),
    );
    let branch_b = HistoryRecord::branch_latest();
    commit(&mut b, &branch_b);
    let context_b = branch_b.context_id();
    assert_eq!(
        b.graph().context_session(context_b).unwrap().messages,
        prefix.messages
    );
    append(
        &mut b,
        context_b,
        &ConversationMessage::user_text("NEXUS_B_PRIVATE"),
    );
    let discarded = append(&mut b, context_b, &text("NEXUS_B_FINISHED"));
    let answer = append(&mut b, context_b, &text("NEXUS_B_FINISHED"));
    let mut compacted = b.graph().context_session(context_b).unwrap();
    compacted.messages = vec![
        ConversationMessage::user_text("NEXUS_B_COMPACTED"),
        text("NEXUS_B_FINISHED"),
    ];
    compacted.record_compaction("B summary", 3);
    let candidate = b
        .graph()
        .prepare_projection(
            context_b,
            b.graph().revision(context_b).unwrap(),
            &compacted,
            &[None, Some(&answer)],
        )
        .unwrap();
    commit(&mut b, &candidate);
    assert_eq!(
        b.graph().message_ids(context_b).unwrap().last(),
        Some(&answer)
    );
    assert!(!b
        .graph()
        .message_ids(context_b)
        .unwrap()
        .contains(&discarded));
    let b_commit = finish_turn(&mut b, context_b, "nexus-b-trace");
    assert!(!fixture.0.join("actual-tool-result").exists());
    std::fs::write(fixture.0.join("tool-release"), b"continue").unwrap();
    assert!(tool.0.wait().unwrap().success());
    let output = std::fs::read_to_string(fixture.0.join("actual-tool-result")).unwrap();
    let result_id = append(
        &mut a,
        context_a,
        &ConversationMessage::tool_result("nexus-call-a", "fixture_tool", output, false),
    );
    append(&mut a, context_a, &text("NEXUS_A_FINISHED"));
    let a_commit = finish_turn(&mut a, context_a, "nexus-a-trace");
    assert_eq!(
        a.graph().tool_origin(&result_id, "nexus-call-a"),
        Some(assistant_id.as_str())
    );
    let receipt = std::env::var("NEXUS_HISTORY_RECEIPT").expect("history restart receipt path");
    std::fs::write(
        receipt,
        serde_json::to_vec(&json!({
            "path":path, "sid":prefix.session_id, "a_context":context_a, "b_context":context_b,
            "a_commit":a_commit, "b_commit":b_commit, "assistant":assistant_id, "result":result_id,
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "real daemon restart; e2e/nexus-a2a/run.sh owns stop/start"]
fn live_shared_history_restores_both_contexts_after_restart() {
    let receipt = std::env::var("NEXUS_HISTORY_RECEIPT").expect("history restart receipt path");
    let before: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    let mut log = HistoryLog::new(
        live_backend()
            .record_log(before["path"].as_str().unwrap(), RecordLogMode::Existing)
            .unwrap(),
    );
    log.refresh().unwrap();
    let a = log
        .graph()
        .completed_session(Some(before["a_commit"].as_str().unwrap()))
        .unwrap();
    assert_eq!(a.session_id, before["sid"]);
    assert!(a.to_json().unwrap().render().contains("REAL_TOOL_A_RESULT"));
    assert!(!a.to_json().unwrap().render().contains("NEXUS_B_COMPACTED"));
    let b = log
        .graph()
        .completed_session(Some(before["b_commit"].as_str().unwrap()))
        .unwrap();
    assert_eq!(b.messages.len(), 2);
    assert!(b.to_json().unwrap().render().contains("NEXUS_B_COMPACTED"));
    assert!(!b.to_json().unwrap().render().contains("NEXUS_A_PRIVATE"));
    assert_eq!(
        log.graph()
            .tool_origin(before["result"].as_str().unwrap(), "nexus-call-a"),
        before["assistant"].as_str()
    );
    let branch = HistoryRecord::branch_latest();
    commit(&mut log, &branch);
    assert_eq!(
        log.graph()
            .context_session(branch.context_id())
            .unwrap()
            .messages,
        a.messages
    );
    append(
        &mut log,
        branch.context_id(),
        &ConversationMessage::user_text("AFTER_NEXUS_RESTART"),
    );
    append(&mut log, branch.context_id(), &text("POST_RESTART_ANSWER"));
    finish_turn(&mut log, branch.context_id(), "after-restart-trace");
    std::fs::write(
        Path::new(&receipt).with_extension("after-restart.json"),
        serde_json::to_vec(
            &json!({"result":"PASS", "sid":a.session_id, "retained_branches":2,
            "post_restart_commit":log.graph().latest_commit()}),
        )
        .unwrap(),
    )
    .unwrap();
}
