//! Model discovery, copied send address and durable peer delivery through a real PTY.
mod common;

use std::time::Duration;

use common::TestEnv;
use mock_anthropic_service::{AGENT_DIRECTORY_BODY, UNIFIED_SEND_RECIPIENT};
use runtime::mailbox::{local_pair_root_in, Mailbox};

#[test]
fn model_copies_agent_list_address_and_delivers_to_the_peer() {
    let env = TestEnv::new("agent-directory-address");
    let peer = Mailbox::workspace_local(
        &local_pair_root_in(env.config_home()),
        UNIFIED_SEND_RECIPIENT.into(),
    );
    peer.ensure_presence().unwrap();
    let mut cli = env.spawn(&["--permission-mode", "danger-full-access"]);
    cli.resize(50, 120).unwrap();
    common::expect_input_line_cleared(&cli, env.timeout(), "directory ready");
    let before = common::turn_status_marker(&cli);
    let prompt = env.prompt(
        &format!("Discover the reachable Agents. Copy the address returned for {UNIFIED_SEND_RECIPIENT} and send it this message: {AGENT_DIRECTORY_BODY}"),
        "agent_directory_roundtrip",
    );
    cli.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(
        &cli,
        &before,
        Duration::from_secs(90),
        "discovery and send",
    );
    let conversations = peer.list_conversations().unwrap();
    if conversations.is_empty() && env.is_mock() {
        for body in env.captured_message_bodies() {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            eprintln!("Model messages: {}", body["messages"]);
        }
        eprintln!("PTY: {}", cli.render(|screen| screen.contents()));
    }
    assert_eq!(
        conversations.len(),
        1,
        "send must address one discovered peer"
    );
    let (messages, _) = peer.poll_conversation(&conversations[0], 0, 0).unwrap();
    assert!(messages
        .iter()
        .any(|message| message.body == AGENT_DIRECTORY_BODY));
    assert!(messages
        .iter()
        .all(|message| message.to == UNIFIED_SEND_RECIPIENT));
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
}
