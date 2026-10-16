//! ACP wire adaptation over one persistent engine command/event handle.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol_schema::{ClientRequest, ExtRequest};
use engine_core::engine_events::{CompletionAction, CompletionReceipt};
use engine_core::{EngineAttachment, EngineCommand, EngineEvent, EngineHandle, EngineSession};
use engine_host::SessionEngine;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::acp_sdk_server::SessionSubagents;
use crate::session_ops;

type ControlWaiter = std::sync::mpsc::Sender<Result<Value, String>>;

pub(crate) struct SessionDriver {
    commands: std::sync::mpsc::Sender<EngineCommand>,
    handle: Mutex<Option<EngineHandle>>,
    attachment: EngineAttachment,
    listener: Mutex<Option<UnboundedSender<EngineEvent>>>,
    connection: Mutex<Option<ConnectionTo<Client>>>,
    subagents: Mutex<Option<Weak<SessionSubagents>>>,
    requests: Mutex<HashMap<u64, ControlWaiter>>,
    next_request: AtomicU64,
    tasks: AtomicBool,
    transport: Mutex<Option<tokio::task::JoinHandle<()>>>,
    keepalive: Mutex<Option<Arc<Self>>>,
    bridge_stopped: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SessionDriver {
    fn new(
        (mut handle, attachment): (EngineHandle, EngineAttachment),
        session_id: String,
    ) -> Arc<Self> {
        let (_, unused) = std::sync::mpsc::channel();
        let events = std::mem::replace(&mut handle.events, unused);
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let driver = Arc::new(Self {
            commands: handle.commands.clone(),
            handle: Mutex::new(Some(handle)),
            attachment,
            listener: Mutex::new(None),
            connection: Mutex::new(None),
            subagents: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            tasks: AtomicBool::new(false),
            transport: Mutex::new(None),
            keepalive: Mutex::new(None),
            bridge_stopped: Mutex::new(stopped_rx),
        });
        let (tx, mut rx) = unbounded_channel();
        std::thread::Builder::new()
            .name("acp-engine-events".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn ACP engine event bridge");
        let weak = Arc::downgrade(&driver);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let Some(driver) = weak.upgrade() else {
                    break;
                };
                driver.on_event(&session_id, event);
            }
            let _ = stopped_tx.send(());
        });
        driver
    }

    fn on_event(self: &Arc<Self>, session_id: &str, event: EngineEvent) {
        match event {
            EngineEvent::CommandResult { id, result } => {
                if let Some(waiter) = self
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = waiter.send(result);
                }
                return;
            }
            EngineEvent::CompletionRequest {
                attempt,
                tasks,
                payload,
            } => {
                self.deliver(attempt, tasks, payload);
                return;
            }
            EngineEvent::CompletionIdle => {
                self.abort_transport();
                self.keepalive
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                return;
            }
            _ => {}
        }
        let route = self
            .subagents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        let turn_event = matches!(
            &event,
            EngineEvent::TextDelta { .. }
                | EngineEvent::State(_)
                | EngineEvent::PermissionRequest { .. }
                | EngineEvent::QuestionRequest { .. }
                | EngineEvent::ContextCommitted(_)
                | EngineEvent::TurnComplete(_)
                | EngineEvent::TurnFailed { .. }
                | EngineEvent::PromptRejected { .. }
                | EngineEvent::Error { .. }
        )
        .then(|| event.clone());
        if let Some(notification) =
            session_ops::engine_event_to_session_update(session_id, event, route.is_some())
        {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(connection) = connection {
                let _ = connection.send_notification(notification);
            }
        }
        if let Some(event) = turn_event {
            if let Some(listener) = self
                .listener
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                let _ = listener.send(event);
            }
        }
    }

    pub(crate) fn open(
        session: &runtime::Session,
        cwd: &Path,
        create: bool,
    ) -> Result<Arc<Self>, String> {
        Ok(Self::new(
            engine_host::session::completion_engine(session, cwd, create)?,
            session.session_id.clone(),
        ))
    }

    pub(crate) fn stored(session_id: &str, cwd: &Path) -> Result<Arc<Self>, String> {
        Ok(Self::new(
            engine_host::session::stored_completion_engine(session_id, cwd)?,
            session_id.into(),
        ))
    }

    pub(crate) fn start(
        engine: Arc<SessionEngine>,
        subagents: Option<&Arc<SessionSubagents>>,
        session_id: String,
        existing: Option<Arc<Self>>,
    ) -> Result<Arc<Self>, String> {
        let driver = subagents
            .and_then(|route| route.completion_delivery())
            .or(existing)
            .unwrap_or_else(|| Self::new(EngineSession::deferred(Arc::default()), session_id));
        driver.attachment.attach(engine)?;
        *driver
            .subagents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = subagents.map(Arc::downgrade);
        Ok(driver)
    }

    pub(crate) fn attach_session(
        &self,
        session: &runtime::Session,
        cwd: &Path,
    ) -> Result<(), String> {
        self.attachment.attach_session(session, cwd)
    }

    pub(crate) fn control(&self, action: CompletionAction) -> Result<Value, String> {
        self.request(|id| EngineCommand::Completion {
            id: Some(id),
            action,
        })
    }

    fn request(&self, command: impl FnOnce(u64) -> EngineCommand) -> Result<Value, String> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        if let Err(error) = self.send(command(id)) {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|e| e.to_string());
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        result?
    }

    pub(crate) async fn control_async(
        self: &Arc<Self>,
        action: CompletionAction,
    ) -> Result<Value, String> {
        let driver = Arc::clone(self);
        tokio::task::spawn_blocking(move || driver.control(action))
            .await
            .map_err(|e| e.to_string())?
    }

    pub(crate) async fn cancel_subagent(
        self: &Arc<Self>,
        agent_id: String,
    ) -> Result<bool, String> {
        let driver = Arc::clone(self);
        let value = tokio::task::spawn_blocking(move || {
            driver.request(|request_id| EngineCommand::CancelSubagent {
                request_id,
                agent_id,
            })
        })
        .await
        .map_err(|e| e.to_string())??;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    pub(crate) fn set_task_protocol(&self, tasks: bool) {
        self.tasks.store(tasks, Ordering::Release);
        let connected = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        let _ = self.send(EngineCommand::Completion {
            id: None,
            action: CompletionAction::Configure { tasks, connected },
        });
    }

    pub(crate) fn bind(self: &Arc<Self>, connection: ConnectionTo<Client>) {
        self.abort_transport();
        *self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(connection);
        *self
            .keepalive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(self));
        let _ = self.send(EngineCommand::Completion {
            id: None,
            action: CompletionAction::Configure {
                tasks: self.tasks.load(Ordering::Acquire),
                connected: true,
            },
        });
    }

    pub(crate) fn unbind(&self) {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.abort_transport();
        self.keepalive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let _ = self.send(EngineCommand::Completion {
            id: None,
            action: CompletionAction::Configure {
                tasks: self.tasks.load(Ordering::Acquire),
                connected: false,
            },
        });
    }

    fn abort_transport(&self) {
        if let Some(task) = self
            .transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    fn deliver(
        self: &Arc<Self>,
        attempt: u64,
        tasks: bool,
        payload: engine_core::engine_events::TaskCompletion,
    ) {
        self.abort_transport();
        *self
            .keepalive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(self));
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let driver = Arc::clone(self);
        let task = tokio::spawn(async move {
            let receipt = match (
                connection,
                payload.wire_value(tasks).and_then(|value| {
                    serde_json::value::to_raw_value(&value).map_err(|e| e.to_string())
                }),
            ) {
                (Some(connection), Ok(raw)) => {
                    let result = connection
                        .send_request(ClientRequest::ExtMethodRequest(ExtRequest::new(
                            if tasks {
                                "_sudocode/task/completed"
                            } else {
                                "_sudocode/subagent/completed"
                            },
                            Arc::from(raw),
                        )))
                        .block_task()
                        .await;
                    match result {
                        Ok(value) => CompletionReceipt::Ack {
                            completion_id: value
                                .get("completionId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            disposition: value
                                .get("disposition")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            reason: value
                                .get("reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        },
                        Err(error) => match error.code {
                            agent_client_protocol_schema::ErrorCode::InvalidParams
                            | agent_client_protocol_schema::ErrorCode::InvalidRequest => {
                                CompletionReceipt::ProtocolRejected
                            }
                            agent_client_protocol_schema::ErrorCode::MethodNotFound
                            | agent_client_protocol_schema::ErrorCode::AuthRequired => {
                                CompletionReceipt::Unavailable
                            }
                            _ => CompletionReceipt::Retry,
                        },
                    }
                }
                _ => CompletionReceipt::Retry,
            };
            let _ = driver.send(EngineCommand::Completion {
                id: None,
                action: CompletionAction::Receipt { attempt, receipt },
            });
        });
        *self
            .transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }

    pub(crate) fn listen(
        self: &Arc<Self>,
        connection: ConnectionTo<Client>,
    ) -> (TurnListener, UnboundedReceiver<EngineEvent>) {
        *self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(connection);
        let (tx, rx) = unbounded_channel();
        let mut listener = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            listener.is_none(),
            "session lane must serialize turn listeners"
        );
        *listener = Some(tx);
        (TurnListener(Arc::clone(self)), rx)
    }

    pub(crate) fn send(&self, command: EngineCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "engine session closed".into())
    }

    pub(crate) fn shutdown(&self) -> bool {
        self.unbind();
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let reaped = handle.is_none_or(|handle| handle.shutdown(Duration::from_secs(30)));
        self.attachment.release();
        let stopped = self
            .bridge_stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(30))
            .is_ok();
        reaped && stopped
    }
}

impl Drop for SessionDriver {
    fn drop(&mut self) {
        self.abort_transport();
    }
}

pub(crate) struct TurnListener(Arc<SessionDriver>);
impl Drop for TurnListener {
    fn drop(&mut self) {
        self.0
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}
