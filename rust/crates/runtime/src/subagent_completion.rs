//! Durable handoff of direct sub-agent results. This is delivery state only;
//! the child's manifest remains the authority for its execution outcome.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_backend::FsBackend;
use crate::{ContentBlock, Session, SubagentLifecycle, SubagentPhase};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Worker {
    agent_id: String,
    background: bool,
    finished: Option<SubagentLifecycle>,
}

/// Shared by the CLI turn pump and ACP's durable publisher. Starting and
/// closing a turn are explicit: ACP does not receive the CLI pump's events.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Completions {
    active: bool,
    workers: BTreeMap<String, Worker>,
    collected: Vec<String>,
}

impl Completions {
    pub fn begin(&mut self) {
        self.active = true;
    }

    pub fn lifecycle(&mut self, event: &SubagentLifecycle) {
        match event.phase {
            SubagentPhase::Started => {
                self.workers
                    .entry(event.tool_call_id.clone())
                    .or_insert_with(|| Worker {
                        agent_id: event.agent.agent_id.clone(),
                        ..Worker::default()
                    });
            }
            SubagentPhase::Finished => {
                if let Some(worker) = self.workers.get_mut(&event.tool_call_id) {
                    worker.finished = Some(event.clone());
                }
                self.suppress_collected();
            }
        }
    }

    pub fn tool_result(&mut self, id: &str, name: &str, output: &str) {
        let value = serde_json::from_str::<serde_json::Value>(output).ok();
        if let Some(worker) = self.workers.get_mut(id) {
            if value
                .as_ref()
                .is_some_and(|v| matches!(v["status"].as_str(), Some("running" | "backgrounded")))
            {
                worker.background = true;
            } else {
                self.workers.remove(id);
            }
        }
        // Only an explicit read consumes a result. An unrelated tool returning
        // similarly shaped JSON must not erase a pending completion.
        if crate::tool_names::canonicalize_tool_name(name) == "pid_output" {
            if let Some(value) = value {
                if matches!(
                    value["status"].as_str(),
                    Some("completed" | "failed" | "killed" | "cancelled")
                ) {
                    if let Some(agent_id) = value["agentId"]
                        .as_str()
                        .or_else(|| value["agent_id"].as_str())
                    {
                        self.workers.retain(|_, worker| worker.agent_id != agent_id);
                    }
                }
            } else if output.starts_with("<task-notification>") {
                self.collected.push(output.to_owned());
                self.suppress_collected();
            }
        }
    }

    fn suppress_collected(&mut self) {
        self.workers.retain(|_, worker| {
            !worker
                .finished
                .as_ref()
                .and_then(|event| event.completion_notification.as_ref())
                .is_some_and(|text| self.collected.contains(text))
        });
    }

    pub fn close(&mut self) {
        self.active = false;
    }

    pub fn drain(&mut self) -> Vec<SubagentLifecycle> {
        if self.active {
            return Vec::new();
        }
        let mut ready = Vec::new();
        self.workers.retain(|_, worker| {
            if worker.background {
                if let Some(finished) = worker.finished.take() {
                    ready.push(finished);
                    return false;
                }
            }
            true
        });
        if self.workers.is_empty() {
            self.collected.clear();
        }
        ready
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRef {
    pub session_id: String,
    pub revision: u64,
    pub digest: String,
    pub last_trace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Completion {
    pub session_id: String,
    pub completion_id: String,
    pub agent_id: String,
    pub parent_tool_call_id: String,
    pub origin_trace_id: String,
    pub status: String,
    pub message: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Source {
    ordinal: u64,
    lifecycle: SubagentLifecycle,
    trace_id: String,
    manifest_path: String,
    closed: bool,
    settled: bool,
    payload: Option<Completion>,
    ack: Option<String>,
    blocked: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    watermark: u64,
    session_id: String,
    cwd: PathBuf,
    context: Option<ContextRef>,
    active_trace: Option<String>,
    reducer: Completions,
    sources: BTreeMap<String, Source>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStatus {
    pub watermark: u64,
    pub waiting: usize,
    pub pending: usize,
    pub unresolved: usize,
    pub settled: usize,
    pub blocked: Vec<String>,
    pub context_ref: Option<ContextRef>,
}

/// One in-process writer per exact session, shared by registration, event
/// observation and ACK handling. No lock is held while waiting for an ACK.
pub struct CompletionJournal {
    fs: Arc<dyn FsBackend>,
    path: String,
    transcript: PathBuf,
    state: Mutex<Journal>,
}

impl CompletionJournal {
    pub fn open(session: &Session, cwd: &Path, create: bool) -> Result<Arc<Self>, String> {
        let transcript = session
            .persistence_path()
            .ok_or("session has no transcript")?
            .to_path_buf();
        let fs = session.fs_handle();
        let path = transcript
            .with_extension("completions.json")
            .to_string_lossy()
            .into_owned();
        let state = if fs.exists(&path).map_err(|e| e.to_string())? {
            let state: Journal =
                serde_json::from_str(&fs.read_to_string(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if state.version != 1 || state.session_id != session.session_id || state.cwd != cwd {
                return Err("completion journal identity mismatch".into());
            }
            state
        } else if create {
            Journal {
                version: 1,
                watermark: 0,
                session_id: session.session_id.clone(),
                cwd: cwd.to_path_buf(),
                context: None,
                active_trace: None,
                reducer: Completions::default(),
                sources: BTreeMap::new(),
            }
        } else {
            return Err("completion journal missing".into());
        };
        let interrupted = state.active_trace.clone();
        let journal = Arc::new(Self {
            fs,
            path,
            transcript,
            state: Mutex::new(state),
        });
        // Opening a new writer means the previous process no longer owns this
        // journal. Its interrupted turn cannot publish, but must not prevent a
        // later explicit human turn from repairing the conversation.
        if let Some(trace) = interrupted {
            journal.fail_turn(&trace)?;
        }
        if create {
            journal.change(|_| Ok(()))?;
        }
        Ok(journal)
    }

    fn change<T>(&self, f: impl FnOnce(&mut Journal) -> Result<T, String>) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "completion journal poisoned")?;
        // A failed write must not leave an in-memory ACK/publication that the
        // next mutation could silently treat as durable.
        let mut next = state.clone();
        next.watermark += 1;
        let result = f(&mut next)?;
        let bytes = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err("completion_journal_capacity: exceeds 64 MiB".into());
        }
        self.fs
            .write_atomic(&self.path, &bytes)
            .map_err(|e| e.to_string())?;
        *state = next;
        Ok(result)
    }

    pub fn matches_identity(&self, session: &Session, cwd: &Path) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.session_id == session.session_id
            && state.cwd == cwd
            && session.persistence_path() == Some(self.transcript.as_path())
    }

    pub fn begin(&self, trace: &str, expected: Option<&ContextRef>) -> Result<(), String> {
        if trace.is_empty() {
            return Err("completion prompt requires traceId".into());
        }
        let digest = self.digest()?;
        self.change(|state| {
            if let Some(expected) = expected {
                if state.context.as_ref() != Some(expected)
                    || digest != expected.digest
                    || state.active_trace.is_some()
                {
                    return Err(
                        "[context_mismatch] expectedContext does not match committed session"
                            .into(),
                    );
                }
            }
            if state.active_trace.is_some() {
                return Err("source_context_unconfirmed".into());
            }
            state.active_trace = Some(trace.to_owned());
            state.reducer.begin();
            Ok(())
        })
    }

    /// Called synchronously BEFORE the child executes. A storage failure is a
    /// tool error and must prevent the spawn, including synchronous children.
    pub fn register(
        &self,
        lifecycle: SubagentLifecycle,
        manifest_path: String,
    ) -> Result<(), String> {
        self.change(|state| {
            let trace_id = state
                .active_trace
                .clone()
                .ok_or("no active completion source")?;
            if state.sources.len() >= 10000 {
                return Err(
                    "completion_journal_capacity: session has 10000 registered children".into(),
                );
            }
            if state.sources.contains_key(&lifecycle.tool_call_id) {
                return Err("duplicate spawn tool id".into());
            }
            state.reducer.lifecycle(&lifecycle);
            state.sources.insert(
                lifecycle.tool_call_id.clone(),
                Source {
                    ordinal: 0,
                    lifecycle,
                    trace_id,
                    manifest_path,
                    closed: false,
                    settled: false,
                    payload: None,
                    ack: None,
                    blocked: None,
                },
            );
            Ok(())
        })
    }

    pub fn lifecycle(&self, event: &SubagentLifecycle) -> Result<(), String> {
        if event.completion_notification.is_none()
            || event.phase == SubagentPhase::Started
            || !matches!(
                event.status.as_deref(),
                Some("completed" | "failed" | "killed" | "cancelled")
            )
        {
            return Ok(());
        } // registration is already durable
        self.change(|state| {
            if !state.sources.contains_key(&event.tool_call_id) {
                return Ok(());
            }
            if let Some(source) = state.sources.get_mut(&event.tool_call_id) {
                if source.payload.is_some() || source.settled {
                    return Ok(());
                }
                if source.closed {
                    source.blocked = None;
                }
            }
            let mut bounded = event.clone();
            bounded.raw_output = None;
            state.reducer.lifecycle(&bounded);
            Self::publish(state)
        })
    }

    pub fn tool_result(&self, id: &str, name: &str, output: &str) -> Result<(), String> {
        self.change(|state| {
            state.reducer.tool_result(id, name, output);
            Ok(())
        })
    }

    /// Called only after the turn event channel drains AND the transcript is
    /// committed. The context digest covers the persisted, recoverable bytes.
    pub fn commit(&self, trace: &str) -> Result<ContextRef, String> {
        let digest = self.digest()?;
        self.change(|state| {
            if state.active_trace.as_deref() != Some(trace) {
                return Err("completion turn identity mismatch".into());
            }
            let context = ContextRef {
                session_id: state.session_id.clone(),
                revision: state.context.as_ref().map_or(1, |c| c.revision + 1),
                digest,
                last_trace_id: trace.to_owned(),
            };
            state.context = Some(context.clone());
            state.active_trace = None;
            for source in state.sources.values_mut().filter(|s| s.trace_id == trace) {
                source.closed = true;
            }
            state.reducer.close();
            Self::publish(state)?;
            Ok(context)
        })
    }

    pub fn fail_turn(&self, trace: &str) -> Result<(), String> {
        self.change(|state| {
            if state.active_trace.as_deref() == Some(trace) {
                state.active_trace = None;
                state.context = None;
                state.reducer.close();
                for source in state.sources.values_mut().filter(|s| s.trace_id == trace) {
                    source.blocked = Some("source_context_unconfirmed".into());
                }
                // Keep those workers inert; a failed parent did not commit a
                // delivery boundary. A later human turn can still proceed.
                state
                    .reducer
                    .workers
                    .retain(|tool, _| state.sources.get(tool).is_none_or(|s| s.trace_id != trace));
            }
            Ok(())
        })
    }

    pub fn active(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_trace
            .is_some()
    }

    fn publish(state: &mut Journal) -> Result<(), String> {
        for event in state.reducer.drain() {
            let source = state
                .sources
                .get_mut(&event.tool_call_id)
                .ok_or("completion source missing")?;
            if !source.closed || source.payload.is_some() || source.settled {
                continue;
            }
            let message = event
                .completion_notification
                .ok_or("terminal completion text missing")?;
            if message.len() > 65536 {
                return Err("completion message exceeds 64 KiB".into());
            }
            let id = checksum(
                &serde_json::to_vec(&(
                    1,
                    &state.session_id,
                    &event.tool_call_id,
                    &event.agent.agent_id,
                ))
                .map_err(|e| e.to_string())?,
            );
            // Order published results, not spawns: an earlier child finishing
            // late must not overtake a frozen delivery awaiting its durable ACK,
            // including when the sender restarts between attempts.
            source.ordinal = state.watermark;
            source.payload = Some(Completion {
                session_id: state.session_id.clone(),
                completion_id: id,
                agent_id: event.agent.agent_id,
                parent_tool_call_id: event.tool_call_id,
                origin_trace_id: source.trace_id.clone(),
                status: match event.status.as_deref() {
                    Some("completed") => "completed",
                    Some("cancelled" | "killed") => "cancelled",
                    _ => "failed",
                }
                .into(),
                message,
            });
        }
        for (tool, source) in &mut state.sources {
            if source.closed
                && source.blocked.is_none()
                && source.payload.is_none()
                && !state.reducer.workers.contains_key(tool)
            {
                source.settled = true;
            }
        }
        Ok(())
    }

    fn digest(&self) -> Result<String, String> {
        self.fs
            .read(&self.transcript.to_string_lossy())
            .map(|bytes| checksum(&bytes))
            .map_err(|e| e.to_string())
    }

    pub fn context(&self) -> Result<Option<ContextRef>, String> {
        let digest = self.digest()?;
        let state = self
            .state
            .lock()
            .map_err(|_| "completion journal poisoned")?;
        if state.active_trace.is_some()
            || state.context.as_ref().is_some_and(|c| c.digest != digest)
        {
            return Err("source_context_unconfirmed".into());
        }
        Ok(state.context.clone())
    }

    pub fn pending(&self) -> Vec<Completion> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut sources: Vec<_> = state
            .sources
            .values()
            .filter(|s| s.ack.is_none() && s.blocked.is_none())
            .collect();
        sources.sort_by_key(|s| s.ordinal);
        sources
            .into_iter()
            .filter_map(|s| s.payload.clone())
            .collect()
    }

    pub fn acknowledge(&self, id: &str, disposition: &str) -> Result<(), String> {
        if !matches!(disposition, "stored" | "session_deleted") {
            return Err("invalid completion acknowledgement".into());
        }
        self.change(|state| {
            let source = state
                .sources
                .values_mut()
                .find(|s| s.payload.as_ref().is_some_and(|p| p.completion_id == id))
                .ok_or("unknown completion acknowledgement")?;
            source.ack = Some(disposition.to_owned());
            source.settled = true;
            // Keep identity/ACK tombstones for duplicate suppression while
            // releasing the potentially large result after durable receipt.
            if let Some(payload) = &mut source.payload {
                payload.message.clear();
            }
            source.lifecycle.raw_output = None;
            source.lifecycle.completion_notification = None;
            Ok(())
        })
    }

    pub fn quarantine(&self, id: &str, reason: &str) -> Result<(), String> {
        self.change(|state| {
            let source = state
                .sources
                .values_mut()
                .find(|s| s.payload.as_ref().is_some_and(|p| p.completion_id == id))
                .ok_or("unknown completion")?;
            source.blocked = Some(reason.to_owned());
            Ok(())
        })
    }

    /// Reconcile only registered children. This reads metadata, transcript and
    /// manifests; it never creates an engine or restarts a missing child.
    pub fn reconcile(
        &self,
        terminal: impl Fn(&str, &SubagentLifecycle) -> Result<Option<SubagentLifecycle>, String>,
    ) -> Result<ReplayStatus, String> {
        let session = Session::load_from_path_with(self.fs.as_ref(), &self.transcript)
            .map_err(|e| e.to_string())?;
        let digest = self.digest()?;
        self.change(|state| {
            if state.active_trace.is_some()
                || state.context.is_none()
                || state.context.as_ref().is_some_and(|c| c.digest != digest)
            {
                for source in state
                    .sources
                    .values_mut()
                    .filter(|s| !s.settled && s.payload.is_none())
                {
                    source.blocked = Some("source_context_unconfirmed".into());
                }
            } else {
                // Replaying committed reads before terminal lifecycle events also
                // handles pid_output winning the manifest/event race.
                for message in &session.messages {
                    for block in &message.blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            tool_name,
                            output,
                            ..
                        } = block
                        {
                            state.reducer.tool_result(tool_use_id, tool_name, output);
                        }
                    }
                }
                for source in state
                    .sources
                    .values_mut()
                    .filter(|s| s.closed && !s.settled && s.payload.is_none())
                {
                    match terminal(&source.manifest_path, &source.lifecycle) {
                        Ok(Some(event)) => {
                            source.blocked = None;
                            state.reducer.lifecycle(&event);
                        }
                        Ok(None) => {
                            source.blocked = Some("execution_unconfirmed".into());
                        }
                        Err(_) => {
                            source.blocked = Some("terminal_manifest_unavailable".into());
                        }
                    }
                }
                Self::publish(state)?;
            }
            Ok(Self::status_inner(state))
        })
    }

    pub fn status(&self) -> ReplayStatus {
        Self::status_inner(
            &self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn status_inner(state: &Journal) -> ReplayStatus {
        ReplayStatus {
            watermark: state.watermark,
            waiting: state
                .sources
                .values()
                .filter(|s| !s.settled && s.payload.is_none() && s.blocked.is_none())
                .count(),
            pending: state
                .sources
                .values()
                .filter(|s| s.payload.is_some() && s.ack.is_none() && s.blocked.is_none())
                .count(),
            unresolved: state.sources.values().filter(|s| !s.settled).count(),
            settled: state.sources.values().filter(|s| s.settled).count(),
            blocked: state
                .sources
                .values()
                .filter_map(|s| s.blocked.clone())
                .take(16)
                .collect(),
            context_ref: state.context.clone(),
        }
    }
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubagentIdentity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn lifecycle(phase: SubagentPhase) -> SubagentLifecycle {
        SubagentLifecycle {
            tool_call_id: "spawn-1".into(),
            phase,
            seq: 0,
            agent: SubagentIdentity {
                agent_id: "agent-1".into(),
                name: "worker".into(),
                description: "test".into(),
                subagent_type: None,
                model: None,
                color: None,
                background: false,
            },
            status: (phase == SubagentPhase::Finished).then(|| "completed".into()),
            started_at: None,
            completed_at: None,
            raw_output: None,
            completion_notification: (phase == SubagentPhase::Finished)
                .then(|| "<task-notification>done</task-notification>".into()),
        }
    }

    #[test]
    fn handoff_terminal_and_closed_are_all_required_in_every_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            for status in ["running", "backgrounded", "completed"] {
                let mut reducer = Completions::default();
                reducer.begin();
                reducer.lifecycle(&lifecycle(SubagentPhase::Started));
                let mut count = 0;
                for (step, event) in order.iter().enumerate() {
                    match event {
                        0 => reducer.lifecycle(&lifecycle(SubagentPhase::Finished)),
                        1 => reducer.tool_result(
                            "spawn-1",
                            "agent_spawn",
                            &format!("{{\"status\":\"{status}\"}}"),
                        ),
                        _ => reducer.close(),
                    }
                    count += reducer.drain().len();
                    if step < 2 {
                        assert_eq!(count, 0, "{order:?}, {status}");
                    }
                }
                assert_eq!(count, usize::from(status != "completed"));
                assert!(reducer.drain().is_empty());
            }
        }
    }

    #[test]
    fn explicit_collection_before_publication_suppresses_even_before_finished_event() {
        for terminal_first in [true, false] {
            for output in [
                r#"{"status":"completed","agentId":"agent-1"}"#,
                "<task-notification>done</task-notification>",
            ] {
                let mut reducer = Completions::default();
                reducer.begin();
                reducer.lifecycle(&lifecycle(SubagentPhase::Started));
                reducer.tool_result("spawn-1", "agent_spawn", r#"{"status":"running"}"#);
                if terminal_first {
                    reducer.lifecycle(&lifecycle(SubagentPhase::Finished));
                }
                reducer.tool_result("read-1", "pid_output", output);
                if !terminal_first {
                    reducer.lifecycle(&lifecycle(SubagentPhase::Finished));
                }
                reducer.close();
                assert!(reducer.drain().is_empty());
            }
        }
    }

    #[test]
    fn unrelated_tool_output_cannot_consume_completion() {
        let mut reducer = Completions::default();
        reducer.begin();
        reducer.lifecycle(&lifecycle(SubagentPhase::Started));
        reducer.tool_result("spawn-1", "agent_spawn", r#"{"status":"running"}"#);
        reducer.tool_result(
            "read-1",
            "read_file",
            r#"{"status":"completed","agentId":"agent-1"}"#,
        );
        reducer.lifecycle(&lifecycle(SubagentPhase::Finished));
        reducer.close();
        assert_eq!(reducer.drain().len(), 1);
    }

    struct Store {
        dir: PathBuf,
        session: Session,
    }
    impl Store {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "scode-completion-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let session = Session::new().with_persistence_path(dir.join("session.jsonl"));
            session
                .save_to_path(session.persistence_path().unwrap())
                .unwrap();
            Self { dir, session }
        }
        fn open(&self, create: bool) -> Arc<CompletionJournal> {
            CompletionJournal::open(&self.session, &self.dir, create).unwrap()
        }
        fn source(&self, journal: &CompletionJournal) {
            journal.begin("run:1", None).unwrap();
            journal
                .register(lifecycle(SubagentPhase::Started), "child.json".into())
                .unwrap();
            journal
                .tool_result("spawn-1", "agent_spawn", r#"{"status":"running"}"#)
                .unwrap();
        }
    }
    impl Drop for Store {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn frozen_delivery_replays_identically_until_ack_is_committed() {
        let store = Store::new();
        let journal = store.open(true);
        store.source(&journal);
        journal
            .lifecycle(&lifecycle(SubagentPhase::Finished))
            .unwrap();
        assert!(journal.pending().is_empty());
        journal.commit("run:1").unwrap();
        let payload = journal.pending();
        assert_eq!(payload.len(), 1);
        drop(journal); // crash before receiving/storing the controller ACK
        let journal = store.open(false);
        assert_eq!(journal.pending(), payload);
        journal
            .tool_result(
                "read-1",
                "pid_output",
                r#"{"status":"completed","agentId":"agent-1"}"#,
            )
            .unwrap();
        assert_eq!(journal.pending(), payload); // publication is the cutoff
        journal
            .acknowledge(&payload[0].completion_id, "stored")
            .unwrap();
        drop(journal);
        let journal = store.open(false);
        assert!(journal.pending().is_empty());
        assert_eq!(journal.status().settled, 1);
    }

    #[test]
    fn late_earlier_spawn_cannot_overtake_unacknowledged_completion() {
        let store = Store::new();
        let journal = store.open(true);
        store.source(&journal);
        let mut second = lifecycle(SubagentPhase::Started);
        second.tool_call_id = "spawn-2".into();
        second.agent.agent_id = "agent-2".into();
        journal
            .register(second.clone(), "child-2.json".into())
            .unwrap();
        journal
            .tool_result("spawn-2", "agent_spawn", r#"{"status":"running"}"#)
            .unwrap();
        journal.commit("run:1").unwrap();
        second.phase = SubagentPhase::Finished;
        second.status = Some("completed".into());
        second.completion_notification = Some("second child finished first".into());
        journal.lifecycle(&second).unwrap();
        let first_delivery = journal.pending()[0].clone();
        assert_eq!(first_delivery.agent_id, "agent-2");
        journal
            .lifecycle(&lifecycle(SubagentPhase::Finished))
            .unwrap();
        assert_eq!(journal.pending()[0], first_delivery);
        drop(journal);
        let journal = store.open(false);
        assert_eq!(journal.pending()[0], first_delivery);
        journal
            .acknowledge(&first_delivery.completion_id, "stored")
            .unwrap();
        assert_eq!(journal.pending().len(), 1);
        assert_eq!(journal.pending()[0].agent_id, "agent-1");
    }

    #[test]
    fn terminal_manifest_recovers_a_lost_finished_event_without_an_engine() {
        let store = Store::new();
        let journal = store.open(true);
        store.source(&journal);
        let context = journal.commit("run:1").unwrap();
        drop(journal);
        let journal = store.open(false);
        let status = journal
            .reconcile(|path, source| {
                assert_eq!(path, "child.json");
                assert_eq!(source.agent.agent_id, "agent-1");
                Ok(Some(lifecycle(SubagentPhase::Finished)))
            })
            .unwrap();
        assert_eq!(status.pending, 1);
        assert_eq!(journal.context().unwrap(), Some(context));
    }

    #[test]
    fn crash_before_parent_commit_is_blocked_even_with_terminal_manifest() {
        let store = Store::new();
        let journal = store.open(true);
        store.source(&journal);
        drop(journal);
        let journal = store.open(false);
        let status = journal
            .reconcile(|_, _| panic!("unconfirmed parent must not publish child result"))
            .unwrap();
        assert_eq!(status.blocked, vec!["source_context_unconfirmed"]);
        assert_eq!(status.pending, 0);
        assert_eq!(journal.context().unwrap(), None);
        journal.begin("human:2", None).unwrap();
        journal.commit("human:2").unwrap();
        assert!(journal.pending().is_empty());
    }

    #[test]
    fn missing_terminal_is_unknown_and_does_not_restart_work() {
        let store = Store::new();
        let journal = store.open(true);
        store.source(&journal);
        journal.commit("run:1").unwrap();
        let status = journal.reconcile(|_, _| Ok(None)).unwrap();
        assert_eq!(status.blocked, vec!["execution_unconfirmed"]);
        assert!(journal.pending().is_empty());
        journal
            .lifecycle(&lifecycle(SubagentPhase::Finished))
            .unwrap();
        assert_eq!(journal.pending().len(), 1); // new actual evidence may unblock
    }

    #[test]
    fn expected_context_is_checked_before_next_input_and_survives_reopen() {
        let store = Store::new();
        let journal = store.open(true);
        journal.begin("run:1", None).unwrap();
        let context = journal.commit("run:1").unwrap();
        drop(journal);
        let journal = store.open(false);
        let mut wrong = context.clone();
        wrong.revision += 1;
        assert!(journal.begin("run:2", Some(&wrong)).is_err());
        assert_eq!(journal.context().unwrap(), Some(context.clone()));
        journal.begin("run:2", Some(&context)).unwrap();
        let next = journal.commit("run:2").unwrap();
        assert_eq!(next.revision, context.revision + 1);
        assert_eq!(next.last_trace_id, "run:2");
        assert!(journal.begin("run:3", Some(&context)).is_err());
    }

    #[test]
    fn transcript_change_and_wrong_journal_identity_are_rejected() {
        let store = Store::new();
        assert!(CompletionJournal::open(&store.session, &store.dir, false).is_err());
        let journal = store.open(true);
        journal.begin("run:1", None).unwrap();
        let context = journal.commit("run:1").unwrap();
        assert!(
            CompletionJournal::open(&store.session, &store.dir.join("different"), false).is_err()
        );
        let path = store.session.persistence_path().unwrap();
        let mut bytes = std::fs::read(path).unwrap();
        bytes.push(b'\n');
        std::fs::write(path, bytes).unwrap();
        assert!(journal.context().is_err());
        assert!(journal.begin("run:2", Some(&context)).is_err());
    }

    #[test]
    fn failed_ack_write_does_not_remove_pending_delivery() {
        let store = Store::new();
        let journal = store.open(true);
        store.source(&journal);
        journal
            .lifecycle(&lifecycle(SubagentPhase::Finished))
            .unwrap();
        journal.commit("run:1").unwrap();
        let before = journal.pending();
        let path = PathBuf::from(&journal.path);
        let backup = path.with_extension("saved");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap(); // atomic replacement fails
        assert!(journal
            .acknowledge(&before[0].completion_id, "stored")
            .is_err());
        assert_eq!(journal.pending(), before);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(backup, &path).unwrap();
        drop(journal);
        assert_eq!(store.open(false).pending(), before);
    }
}
