//! Consumer acceptance through the existing real ACP stdio harness.
//! This exercises unavailable node bindings, not successful node enrollment.

#[path = "node_home.rs"]
mod node_home;

use super::{pin_agent_name, set_session_mode, spawn_stdio_client, AcpTestClient, TestWorkspace};
use mock_anthropic_service::MockAnthropicService;
use serde_json::{json, Value};
use std::fs;

async fn expect_tool_error(
    client: &mut AcpTestClient,
    method: &str,
    params: Value,
    expected: &str,
) -> String {
    let (_, response) = client.send_request(method, params).await;
    assert!(
        response.get("error").is_none(),
        "preserve the actual tool result: {response}"
    );
    assert_eq!(response["result"]["isError"], true);
    let content = response["result"]["content"].as_str().unwrap();
    assert!(
        content.contains(expected),
        "expected {expected:?}: {response}"
    );
    content.to_string()
}

fn pending_workspace(endpoint: &str, agent: &str) -> (TestWorkspace, std::path::PathBuf) {
    let workspace = TestWorkspace::new("pending-node-binding");
    workspace.create();
    workspace.write_sudocode_json(endpoint);
    pin_agent_name(&workspace.root, agent);
    let agents = workspace.root.join("node-credentials");
    fs::create_dir_all(&agents).unwrap();
    let run = workspace.home.join(".nexus/run");
    fs::create_dir_all(&run).unwrap();
    fs::write(
        run.join("node.json"),
        json!({"version": 1, "endpoint": "127.0.0.1:1", "agents_dir": agents}).to_string(),
    )
    .unwrap();
    (workspace, agents)
}

#[tokio::test]
async fn a_configured_agent_loads_without_a_bundle_and_resolves_it_in_the_same_process() {
    const AGENT: &str = "pending-node-actor";
    let server = MockAnthropicService::spawn()
        .await
        .expect("start model spy");
    let (workspace, agents) = pending_workspace(&server.base_url(), AGENT);

    let mut client = spawn_stdio_client(&workspace);
    client
        .send_request("initialize", json!({"protocolVersion": 1}))
        .await;
    let (_, created) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root, "mcpServers": [],
            }),
        )
        .await;
    assert!(
        created.get("error").is_none(),
        "ordinary session must load: {created}"
    );
    let peer = &created["result"]["_meta"]["sudocode"]["peer"];
    assert_eq!(peer["agent"], AGENT);
    assert_eq!(
        peer["namespace"], "node",
        "missing bundle is not disk: {created}"
    );
    assert!(peer["error"]
        .as_str()
        .is_some_and(|e| e.contains("ask the node owner to mint")));
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    set_session_mode(&mut client, &sid, "workspace-write").await;

    for (method, params) in [
        (
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": "node-peer", "message": "BEFORE_BUNDLE_DISTINCT_OPERATION"}),
        ),
        ("_sudocode/a2a/agent_list", json!({"sessionId": sid})),
    ] {
        expect_tool_error(&mut client, method, params, "ask the node owner to mint").await;
    }
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    assert!(
        !pair_root.exists(),
        "missing bundle must not create a disk namespace"
    );

    // Local fixture only: no live mint or daemon. Once these files exist, the
    // same loaded session must reach the actual node transport instead of
    // retaining a stale unavailable error or selecting disk.
    node_home::write_untrusted_credential(&agents, AGENT);
    for (method, params) in [
        ("_sudocode/a2a/agent_list", json!({"sessionId": sid})),
        (
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": "node-peer", "message": "AFTER_BUNDLE_DISTINCT_OPERATION"}),
        ),
    ] {
        let content = expect_tool_error(&mut client, method, params, "connect").await;
        assert!(
            !content.contains("ask the node owner to mint"),
            "stale binding: {content}"
        );
    }
    client.shutdown().await;
    assert!(!pair_root.exists(), "recovery must keep the node namespace");
    assert!(!workspace.root.join("conversations").exists());
    assert!(!workspace.root.join(".sudocode-inbox").exists());
    node_home::assert_no_disk_mailboxes_below(&workspace.root);
    assert_eq!(
        server
            .captured_requests()
            .await
            .iter()
            .filter(|r| r.path == "/v1/messages")
            .count(),
        0,
        "loading, errors and direct calls must not ask the model"
    );

    workspace.cleanup();
}
