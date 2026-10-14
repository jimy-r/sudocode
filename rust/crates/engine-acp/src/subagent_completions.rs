//! ACP transport for durable completions. Replay opens journals, never engines.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::{ConnectionTo, JsonRpcRequest, JsonRpcResponse};
use agent_client_protocol_schema::{ClientRequest, ExtRequest};
use engine_core::EngineEvent;
use runtime::subagent_completion::{CompletionJournal, ContextRef, ReplayStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

pub const TASK_META_KEY: &str = "taskCompletions";

pub const META_KEY: &str = "subagentCompletions";

pub fn negotiated(meta: Option<&agent_client_protocol_schema::Meta>, key: &str) -> bool {
    meta.and_then(|m| m.get("sudocode"))
        .and_then(|s| s.get(key))
        .and_then(|c| c.get("version"))
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 1)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/subagent/replay", response = ReplayResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplayRequest {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResponse {
    pub watermark: u64,
    pub waiting: usize,
    pub pending: usize,
    pub unresolved: usize,
    pub settled: usize,
    pub blocked: Vec<String>,
    pub context_ref: Option<ContextRef>,
}

impl From<ReplayStatus> for ReplayResponse {
    fn from(s: ReplayStatus) -> Self {
        Self {
            watermark: s.watermark,
            waiting: s.waiting,
            pending: s.pending,
            unresolved: s.unresolved,
            settled: s.settled,
            blocked: s.blocked,
            context_ref: s.context_ref,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub struct TaskResponse {
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

macro_rules! task_request {
    ($name:ident, $method:tt $(, $field:ident: $ty:ty)*) => {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
        #[request(method = $method, response = TaskResponse)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name {
            pub session_id: String,
            pub cwd: std::path::PathBuf,
            pub storage_ref: Option<String>,
            $(pub $field: $ty,)*
        }
    };
}
task_request!(TaskReplayRequest, "_sudocode/task/replay", after_task_id: Option<String>);
task_request!(TaskSuppressRequest, "_sudocode/task/suppress", origin_trace_id: String);
task_request!(TaskPurgeRequest, "_sudocode/task/purge", cleanup_id: String);

pub enum TaskOperation {
    Replay(Option<String>),
    Suppress(String),
    Purge(String),
}

pub struct Delivery {
    pub journal: Arc<CompletionJournal>,
    connection: Mutex<Option<ConnectionTo<Client>>>,
    sending: AtomicBool,
    generation: AtomicU64,
    wake: Notify,
    runtime: tokio::runtime::Handle,
    failure: Mutex<Option<String>>,
    terminal_retries: Mutex<BTreeMap<String, runtime::SubagentLifecycle>>,
}

impl Delivery {
    pub fn open(session: &runtime::Session, cwd: &Path, create: bool) -> Result<Arc<Self>, String> {
        let delivery = Arc::new(Self {
            journal: CompletionJournal::open(session, cwd, create)?,
            connection: Mutex::new(None),
            sending: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            wake: Notify::new(),
            runtime: tokio::runtime::Handle::current(),
            failure: Mutex::new(None),
            terminal_retries: Mutex::new(BTreeMap::new()),
        });
        let weak = Arc::downgrade(&delivery);
        delivery.journal.set_wake(move || {
            if let Some(delivery) = weak.upgrade() {
                delivery.wake_sender();
            }
        });
        Ok(delivery)
    }

    pub fn replay(&self, session: &runtime::Session) -> Result<ReplayResponse, String> {
        let fs = session.fs_handle();
        self.journal
            .reconcile(|path, source| {
                engine_core::subagent_completion::recover(fs.as_ref(), path, source)
            })
            .map(Into::into)
    }

    pub fn begin(&self, trace: &str, expected: Option<&ContextRef>) -> Result<(), String> {
        self.journal.begin(trace, expected)?;
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }

    pub fn observe(self: &Arc<Self>, event: &EngineEvent) {
        let result = match event {
            EngineEvent::Subagent(event) if event.stream.is_none() => match &event.update {
                runtime::SubagentUpdate::Lifecycle(lifecycle) => {
                    let mut retries = self
                        .terminal_retries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Err(error) = self.journal.lifecycle(lifecycle) {
                        // Terminal manifests are already durable. Keep the bounded
                        // lifecycle until this writer can record it; after a process
                        // loss the cold journal reconciliation reads that manifest.
                        let mut terminal = lifecycle.as_ref().clone();
                        terminal.raw_output = None;
                        retries.insert(lifecycle.tool_call_id.clone(), terminal);
                        eprintln!("[acp-completion] terminal persistence pending: {error}");
                    }
                    drop(retries);
                    self.wake_sender();
                    return;
                }
                _ => return,
            },
            EngineEvent::ToolResult {
                id, name, output, ..
            } => self.journal.tool_result(id, name, output),
            _ => return,
        };
        if let Err(error) = result {
            eprintln!("[acp-completion] event persistence failed: {error}");
            *self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        }
        self.wake_sender();
    }

    pub fn commit(self: &Arc<Self>, trace: &str) -> Result<ContextRef, String> {
        self.retry_terminals()?;
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return Err(format!("completion event persistence failed: {error}"));
        }
        let context = self.journal.commit(trace)?;
        self.wake_sender();
        Ok(context)
    }

    fn retry_terminals(&self) -> Result<(), String> {
        let mut retries = self
            .terminal_retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some((id, event)) = retries.first_key_value() {
            self.journal.lifecycle(event)?;
            let id = id.clone();
            retries.remove(&id);
        }
        Ok(())
    }

    pub fn bind(self: &Arc<Self>, connection: ConnectionTo<Client>) {
        let mut binding = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *binding = Some(connection);
        self.generation.fetch_add(1, Ordering::SeqCst);
        drop(binding);
        self.wake_sender();
    }

    fn wake_sender(self: &Arc<Self>) {
        let binding = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if binding.is_none() {
            return;
        }
        self.wake.notify_one();
        if self.sending.swap(true, Ordering::SeqCst) {
            return;
        }
        drop(binding);
        let delivery = Arc::clone(self);
        self.runtime.spawn(async move {
            delivery.send_loop().await;
        });
    }

    async fn send_loop(&self) {
        let mut backoff = 1;
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            // Register before observing state so a completion at the empty-
            // queue boundary cannot be lost between the read and the wait.
            wake.as_mut().enable();
            if self.retry_terminals().is_err() {
                // Reuse the delivery loop for storage recovery. Replaying a
                // lifecycle is safe during a live turn: the reducer still waits
                // for commit and honors explicit pid_output collection.
                tokio::select! {
                    _ = &mut wake => {},
                    () = tokio::time::sleep(Duration::from_secs(backoff)) => { backoff = (backoff * 2).min(30); }
                }
                continue;
            }
            let tasks = self.journal.tasks_enabled();
            let pending: Vec<_> = self
                .journal
                .pending_tasks()
                .into_iter()
                .filter(|p| {
                    tasks || matches!(p, runtime::subagent_completion::TaskCompletion::Subagent(_))
                })
                .collect();
            let Some(payload) = pending.first() else {
                // Live routes are owned by their SessionSubagents; this task
                // holds no engine. Cold replay with only blocked/settled facts
                // can release its temporary route immediately.
                {
                    let _binding = self
                        .connection
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let status = self.journal.status();
                    if status.pending == 0 && status.waiting == 0 && !self.journal.active() {
                        self.sending.store(false, Ordering::SeqCst);
                        return;
                    }
                }
                wake.await;
                continue;
            };
            let (connection, generation) = {
                let binding = self
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (binding.clone(), self.generation.load(Ordering::SeqCst))
            };
            let Some(connection) = connection else {
                self.sending.store(false, Ordering::SeqCst);
                return;
            };
            let Ok(raw) = payload.wire_value(tasks).and_then(|value| {
                serde_json::value::to_raw_value(&value).map_err(|e| e.to_string())
            }) else {
                self.sending.store(false, Ordering::SeqCst);
                return;
            };
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                connection
                    .send_request(ClientRequest::ExtMethodRequest(ExtRequest::new(
                        if tasks {
                            "_sudocode/task/completed"
                        } else {
                            "_sudocode/subagent/completed"
                        },
                        Arc::from(raw),
                    )))
                    .block_task(),
            )
            .await;
            // Retired connections cannot quarantine or stop a rebound delivery.
            if generation != self.generation.load(Ordering::SeqCst) {
                continue;
            }
            if let Ok(Err(error)) = &result {
                match error.code {
                    agent_client_protocol_schema::ErrorCode::MethodNotFound
                    | agent_client_protocol_schema::ErrorCode::AuthRequired => {
                        let mut binding = self
                            .connection
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if generation != self.generation.load(Ordering::SeqCst) {
                            continue;
                        }
                        *binding = None;
                        self.sending.store(false, Ordering::SeqCst);
                        return;
                    }
                    agent_client_protocol_schema::ErrorCode::InvalidParams
                    | agent_client_protocol_schema::ErrorCode::InvalidRequest => {
                        if let Err(error) =
                            self.journal.quarantine(payload.id(), "protocol_rejected")
                        {
                            eprintln!("[acp-completion] quarantine persistence failed: {error}");
                        } else {
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            if let Ok(Ok(value)) = result {
                let id = value.get("completionId").and_then(Value::as_str);
                let disposition = value.get("disposition").and_then(Value::as_str);
                let ack = match disposition {
                    Some("stored") => Some("stored"),
                    Some("discarded")
                        if value.get("reason").and_then(Value::as_str)
                            == Some("session_deleted") =>
                    {
                        Some("session_deleted")
                    }
                    _ => None,
                };
                if id == Some(payload.id()) {
                    if let Some(ack) = ack {
                        match self.journal.acknowledge(payload.id(), ack) {
                            Ok(()) => {
                                backoff = 1;
                                continue;
                            }
                            Err(error) => {
                                eprintln!("[acp-completion] ACK persistence failed: {error}")
                            }
                        }
                    } else if self.journal.quarantine(payload.id(), "invalid_ack").is_ok() {
                        continue;
                    }
                } else if self
                    .journal
                    .quarantine(payload.id(), "ack_identity_mismatch")
                    .is_ok()
                {
                    continue;
                }
            }
            // A lost reply repeats only the frozen payload, never model work.
            tokio::select! { _ = wake => { backoff = 1; },
            () = tokio::time::sleep(Duration::from_millis(backoff * 1000 + u64::from(payload.id().as_bytes()[0]) * 3)) => { backoff = (backoff * 2).min(30); } }
        }
    }
}
