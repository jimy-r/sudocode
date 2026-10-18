use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{invalid, to_value, HistoryEvent, HistoryRecord, ProjectionMessage, RecordOutcome};
use crate::json::JsonValue;
use crate::{ContentBlock, ConversationMessage, MessageRole, Session};

#[derive(Clone, Default)]
struct MessageChain {
    prefix: Option<Arc<Self>>,
    tail: Vec<String>,
}

impl MessageChain {
    fn append(chain: &mut Arc<Self>, id: String) {
        if chain.tail.len() == 64 {
            *chain = Arc::new(Self {
                prefix: Some(Arc::clone(chain)),
                tail: vec![id],
            });
        } else {
            Arc::make_mut(chain).tail.push(id);
        }
    }

    fn ids(&self) -> Vec<String> {
        let mut chunks = vec![self.tail.as_slice()];
        let mut prefix = self.prefix.as_deref();
        while let Some(chain) = prefix {
            chunks.push(chain.tail.as_slice());
            prefix = chain.prefix.as_deref();
        }
        chunks.into_iter().rev().flatten().cloned().collect()
    }
}

impl Drop for MessageChain {
    fn drop(&mut self) {
        let mut prefix = self.prefix.take();
        while let Some(chain) = prefix {
            match Arc::try_unwrap(chain) {
                Ok(mut chain) => prefix = chain.prefix.take(),
                Err(_) => break,
            }
        }
    }
}

struct MessageNode {
    message: ConversationMessage,
    parent: Option<String>,
    tool_origins: BTreeMap<String, String>,
}

#[derive(Clone)]
struct Projection {
    chain: Arc<MessageChain>,
    metadata: Arc<Value>,
}

struct Context {
    revision: String,
    projection: Projection,
    unsealed: BTreeSet<String>,
    open_tools: BTreeMap<String, String>,
}

type SeenRecord = ([u8; 32], RecordOutcome);

/// Stable messages and completed projections. Streamed updates replace only
/// this context's uncommitted assistant; completed prefixes remain immutable.
#[derive(Default)]
pub struct HistoryGraph {
    session_id: Option<String>,
    messages: HashMap<String, MessageNode>,
    contexts: HashMap<String, Context>,
    commits: HashMap<String, Projection>,
    latest_commit: Option<String>,
    seen: HashMap<String, SeenRecord>,
}

fn digest(record: &HistoryRecord) -> io::Result<[u8; 32]> {
    let mut canonical = serde_json::to_value(record).map_err(io::Error::other)?;
    canonical.sort_all_objects();
    Ok(Sha256::digest(serde_json::to_vec(&canonical).map_err(io::Error::other)?).into())
}

fn decode_message(value: &Value) -> io::Result<ConversationMessage> {
    let value = JsonValue::parse(&value.to_string()).map_err(io::Error::other)?;
    super::super::ConversationMessage::from_json(&value).map_err(io::Error::other)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn rejected(reason: &str) -> RecordOutcome {
    RecordOutcome::Rejected(reason.into())
}

fn touch(projection: &mut Projection, timestamp: u64) {
    let metadata = Arc::make_mut(&mut projection.metadata);
    let before = metadata["updated_at_ms"].as_u64().unwrap_or(0);
    metadata["updated_at_ms"] = Value::from(before.max(timestamp));
}

fn tool_changes(
    message: &ConversationMessage,
    assistant_id: &str,
    open: &BTreeMap<String, String>,
) -> io::Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let mut open = open.clone();
    let mut origins = BTreeMap::new();
    let mut calls = BTreeSet::new();
    for block in &message.blocks {
        match block {
            ContentBlock::ToolUse { id, .. } => {
                if message.role != MessageRole::Assistant
                    || id.is_empty()
                    || id.len() > 512
                    || !calls.insert(id.clone())
                {
                    return Err(invalid("invalid or duplicate tool call"));
                }
                if open.get(id).is_some_and(|origin| origin != assistant_id) {
                    return Err(invalid(
                        "tool call identity already pending for another assistant",
                    ));
                }
                open.insert(id.clone(), assistant_id.into());
            }
            ContentBlock::ToolResult { tool_use_id, .. } => {
                if matches!(message.role, MessageRole::Assistant | MessageRole::System) {
                    return Err(invalid("tool result has an invalid speaker role"));
                }
                let origin = open
                    .remove(tool_use_id)
                    .ok_or_else(|| invalid("tool result has no open originating assistant"))?;
                origins.insert(tool_use_id.clone(), origin);
            }
            _ => {}
        }
    }
    Ok((open, origins))
}

impl HistoryGraph {
    /// The newest completed turn in this reader's folded prefix.
    #[must_use]
    pub fn latest_commit(&self) -> Option<&str> {
        self.latest_commit.as_deref()
    }

    #[must_use]
    pub fn revision(&self, context: &str) -> Option<&str> {
        self.contexts
            .get(context)
            .map(|context| context.revision.as_str())
    }

    #[must_use]
    pub fn tool_origin(&self, message: &str, call: &str) -> Option<&str> {
        self.messages
            .get(message)?
            .tool_origins
            .get(call)
            .map(String::as_str)
    }

    #[must_use]
    pub fn parent(&self, message: &str) -> Option<&str> {
        self.messages.get(message)?.parent.as_deref()
    }

    pub fn outcome(&self, record: &HistoryRecord) -> io::Result<Option<RecordOutcome>> {
        match self.seen.get(record.id()) {
            Some((known, outcome)) if *known == digest(record)? => Ok(Some(outcome.clone())),
            Some(_) => Err(invalid("history record ID reused with different contents")),
            None => Ok(None),
        }
    }

    pub fn apply(&mut self, record: &HistoryRecord) -> io::Result<RecordOutcome> {
        if record.version != 3 || !valid_id(record.id()) || !valid_id(record.context_id()) {
            return Err(invalid("unsupported history record version or identity"));
        }
        if record.expected_revision.is_some()
            && matches!(
                record.event,
                HistoryEvent::Initialize { .. } | HistoryEvent::OpenContext { .. }
            )
        {
            return Err(invalid(
                "initialization and context opening cannot replace a revision",
            ));
        }
        if let Some(outcome) = self.outcome(record)? {
            return Ok(outcome);
        }
        let outcome = match &record.event {
            HistoryEvent::Initialize { session } => self.initialize(record, session)?,
            HistoryEvent::OpenContext { source_commit } => {
                let source_commit = source_commit.as_ref().or(self.latest_commit.as_ref());
                if self.contexts.contains_key(record.context_id()) {
                    rejected("context_already_exists")
                } else if let Some(projection) = source_commit.and_then(|id| self.commits.get(id)) {
                    self.contexts.insert(
                        record.context_id.clone(),
                        Context {
                            revision: record.id.clone(),
                            projection: projection.clone(),
                            unsealed: BTreeSet::new(),
                            open_tools: BTreeMap::new(),
                        },
                    );
                    RecordOutcome::Applied
                } else {
                    rejected("source_turn_not_committed")
                }
            }
            _ => self.change(record)?,
        };
        self.seen
            .insert(record.id.clone(), (digest(record)?, outcome.clone()));
        Ok(outcome)
    }

    fn initialize(&mut self, record: &HistoryRecord, value: &Value) -> io::Result<RecordOutcome> {
        if self.session_id.is_some() {
            return Ok(rejected("history_already_initialized"));
        }
        let parsed = JsonValue::parse(&value.to_string()).map_err(io::Error::other)?;
        let session = Session::from_json(&parsed).map_err(io::Error::other)?;
        if !valid_id(&session.session_id) {
            return Err(invalid("invalid durable session identity"));
        }
        let mut metadata = value.clone();
        metadata
            .as_object_mut()
            .ok_or_else(|| invalid("session metadata must be an object"))?
            .remove("messages");
        let mut chain = Arc::new(MessageChain::default());
        let mut nodes = HashMap::new();
        let mut open = BTreeMap::new();
        for (index, message) in session.messages.iter().enumerate() {
            let mut identity = serde_json::to_value((
                "legacy-message-v1",
                &session.session_id,
                index,
                to_value(message)?,
            ))
            .map_err(io::Error::other)?;
            identity.sort_all_objects();
            let bytes = serde_json::to_vec(&identity).map_err(io::Error::other)?;
            let id = format!("legacy-{:x}", Sha256::digest(bytes));
            let Ok((next_open, origins)) = tool_changes(message, &id, &open) else {
                return Ok(rejected("initial_tool_association_invalid"));
            };
            open = next_open;
            nodes.insert(
                id.clone(),
                MessageNode {
                    message: message.clone(),
                    parent: chain.tail.last().cloned(),
                    tool_origins: origins,
                },
            );
            MessageChain::append(&mut chain, id);
        }
        if !open.is_empty() {
            return Ok(rejected("initial_context_has_unfinished_tools"));
        }
        let projection = Projection {
            chain,
            metadata: Arc::new(metadata),
        };
        self.session_id = Some(session.session_id);
        self.messages = nodes;
        self.contexts.insert(
            record.context_id.clone(),
            Context {
                revision: record.id.clone(),
                projection: projection.clone(),
                unsealed: BTreeSet::new(),
                open_tools: BTreeMap::new(),
            },
        );
        self.commits.insert(record.id.clone(), projection);
        self.latest_commit = Some(record.id.clone());
        Ok(RecordOutcome::Applied)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keep ordered context validation and atomic state mutation together"
    )]
    fn change(&mut self, record: &HistoryRecord) -> io::Result<RecordOutcome> {
        let Some(context) = self.contexts.get_mut(record.context_id()) else {
            return Ok(rejected("context_missing"));
        };
        if record.expected_revision.as_deref() != Some(context.revision.as_str()) {
            return Ok(rejected("context_revision_changed"));
        }
        match &record.event {
            HistoryEvent::Message {
                id,
                parent_id,
                message,
            } => {
                if !valid_id(id) || self.messages.contains_key(id) {
                    return Ok(rejected("message_identity_conflict"));
                }
                if parent_id.as_ref() != context.projection.chain.tail.last() {
                    return Ok(rejected("message_parent_changed"));
                }
                let message = decode_message(message)?;
                let Ok((open, origins)) = tool_changes(&message, id, &context.open_tools) else {
                    return Ok(rejected("tool_association_invalid"));
                };
                self.messages.insert(
                    id.clone(),
                    MessageNode {
                        message,
                        parent: parent_id.clone(),
                        tool_origins: origins,
                    },
                );
                MessageChain::append(&mut context.projection.chain, id.clone());
                context.unsealed.insert(id.clone());
                context.open_tools = open;
            }
            HistoryEvent::AssistantUpdate {
                message_id,
                message,
            } => {
                if !context.unsealed.contains(message_id) {
                    return Ok(rejected("assistant_already_committed_or_inherited"));
                }
                let node = self
                    .messages
                    .get(message_id)
                    .ok_or_else(|| invalid("assistant update target missing"))?;
                let message = decode_message(message)?;
                if node.message.role != MessageRole::Assistant
                    || message.role != MessageRole::Assistant
                {
                    return Ok(rejected("update_target_is_not_assistant"));
                }
                let old_calls: BTreeSet<_> = node
                    .message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, .. } => Some(id),
                        _ => None,
                    })
                    .collect();
                let new_calls: BTreeSet<_> = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, .. } => Some(id),
                        _ => None,
                    })
                    .collect();
                if !old_calls.is_subset(&new_calls)
                    || new_calls.len()
                        != message
                            .blocks
                            .iter()
                            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                            .count()
                    || node
                        .message
                        .blocks
                        .iter()
                        .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                        .any(|block| !message.blocks.contains(block))
                {
                    return Ok(rejected("streamed_tool_call_changed"));
                }
                // Only newly streamed calls affect the pending set. Previously
                // completed calls must not reopen when later text is persisted.
                let mut additions = message.clone();
                additions.blocks.retain(|block| match block {
                    ContentBlock::ToolUse { id, .. } => !old_calls.contains(id),
                    _ => false,
                });
                let Ok((open, _)) = tool_changes(&additions, message_id, &context.open_tools)
                else {
                    return Ok(rejected("tool_association_invalid"));
                };
                self.messages
                    .get_mut(message_id)
                    .expect("validated target")
                    .message = message;
                context.open_tools = open;
            }
            HistoryEvent::Projection { metadata, messages } => {
                if !context.open_tools.is_empty() {
                    return Ok(rejected("context_has_unfinished_tools"));
                }
                if metadata["session_id"].as_str() != self.session_id.as_deref()
                    || !metadata.is_object()
                    || metadata.get("messages").is_some()
                {
                    return Ok(rejected("projection_session_identity_changed"));
                }
                let mut checked_metadata = metadata.clone();
                checked_metadata["messages"] = Value::Array(Vec::new());
                Session::from_json(
                    &JsonValue::parse(&checked_metadata.to_string()).map_err(io::Error::other)?,
                )
                .map_err(io::Error::other)?;
                let source: BTreeSet<_> = context.projection.chain.ids().into_iter().collect();
                let mut ids = BTreeSet::new();
                let mut nodes = Vec::new();
                let mut chain = Arc::new(MessageChain::default());
                let mut open = BTreeMap::new();
                for entry in messages {
                    if !valid_id(&entry.id) || !ids.insert(entry.id.clone()) {
                        return Ok(rejected("projection_message_identity_conflict"));
                    }
                    let message = match &entry.message {
                        Some(value) if !self.messages.contains_key(&entry.id) => {
                            decode_message(value)?
                        }
                        None if source.contains(&entry.id) => self
                            .messages
                            .get(&entry.id)
                            .expect("source node")
                            .message
                            .clone(),
                        _ => return Ok(rejected("projection_source_message_changed")),
                    };
                    let Ok((next_open, origins)) = tool_changes(&message, &entry.id, &open) else {
                        return Ok(rejected("projection_tool_association_invalid"));
                    };
                    open = next_open;
                    if entry.message.is_none()
                        && self
                            .messages
                            .get(&entry.id)
                            .expect("retained node")
                            .tool_origins
                            != origins
                    {
                        return Ok(rejected("projection_tool_association_changed"));
                    }
                    if entry.message.is_some() {
                        nodes.push((
                            entry.id.clone(),
                            MessageNode {
                                message,
                                parent: chain.tail.last().cloned(),
                                tool_origins: origins,
                            },
                        ));
                    }
                    MessageChain::append(&mut chain, entry.id.clone());
                }
                if !open.is_empty() {
                    return Ok(rejected("projection_has_unfinished_tools"));
                }
                context.unsealed.retain(|id| ids.contains(id));
                for (id, node) in nodes {
                    context.unsealed.insert(id.clone());
                    self.messages.insert(id, node);
                }
                context.projection = Projection {
                    chain,
                    metadata: Arc::new(metadata.clone()),
                };
            }
            HistoryEvent::CommitTurn { trace_id } => {
                if trace_id.is_empty() || trace_id.len() > 512 {
                    return Ok(rejected("completed_turn_identity_invalid"));
                }
                if !context.open_tools.is_empty() {
                    return Ok(rejected("context_has_unfinished_tools"));
                }
            }
            HistoryEvent::Initialize { .. } | HistoryEvent::OpenContext { .. } => unreachable!(),
        }
        touch(&mut context.projection, record.timestamp_ms);
        context.revision.clone_from(&record.id);
        if matches!(record.event, HistoryEvent::CommitTurn { .. }) {
            self.commits
                .insert(record.id.clone(), context.projection.clone());
            self.latest_commit = Some(record.id.clone());
            context.unsealed.clear();
        }
        Ok(RecordOutcome::Applied)
    }

    pub fn message_ids(&self, context: &str) -> io::Result<Vec<String>> {
        Ok(self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?
            .projection
            .chain
            .ids())
    }

    fn session(&self, projection: &Projection) -> io::Result<Session> {
        let mut value = (*projection.metadata).clone();
        value["messages"] = Value::Array(
            projection
                .chain
                .ids()
                .iter()
                .map(|id| to_value(&self.messages.get(id).expect("indexed message").message))
                .collect::<io::Result<_>>()?,
        );
        Session::from_json(&JsonValue::parse(&value.to_string()).map_err(io::Error::other)?)
            .map_err(io::Error::other)
    }

    pub fn completed_session(&self, commit: Option<&str>) -> io::Result<Session> {
        let commit = commit
            .or(self.latest_commit())
            .ok_or_else(|| invalid("no completed context"))?;
        self.session(
            self.commits
                .get(commit)
                .ok_or_else(|| invalid("completed context missing"))?,
        )
    }

    pub fn context_session(&self, context: &str) -> io::Result<Session> {
        self.session(
            &self
                .contexts
                .get(context)
                .ok_or_else(|| invalid("context missing"))?
                .projection,
        )
    }

    pub fn prepare_message(
        &self,
        context: &str,
        message: &ConversationMessage,
    ) -> io::Result<HistoryRecord> {
        let current = self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?;
        Ok(HistoryRecord::new(
            context.into(),
            Some(current.revision.clone()),
            HistoryEvent::Message {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: current.projection.chain.tail.last().cloned(),
                message: to_value(message)?,
            },
        ))
    }

    pub fn prepare_projection(
        &self,
        context: &str,
        source_revision: &str,
        session: &Session,
    ) -> io::Result<HistoryRecord> {
        let current = self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?;
        if current.revision != source_revision {
            return Err(invalid("compaction source context changed"));
        }
        let mut retained: HashMap<[u8; 32], VecDeque<String>> = HashMap::new();
        for id in current.projection.chain.ids() {
            let message = &self.messages.get(&id).expect("source message").message;
            retained
                .entry(Sha256::digest(message.to_json().render().as_bytes()).into())
                .or_default()
                .push_back(id);
        }
        let messages = session
            .messages
            .iter()
            .map(|message| {
                let fingerprint: [u8; 32] =
                    Sha256::digest(message.to_json().render().as_bytes()).into();
                match retained.get_mut(&fingerprint).and_then(VecDeque::pop_front) {
                    Some(id) => Ok(ProjectionMessage { id, message: None }),
                    None => Ok(ProjectionMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        message: Some(to_value(message)?),
                    }),
                }
            })
            .collect::<io::Result<_>>()?;
        let mut metadata: Value =
            serde_json::from_str(&session.to_json().map_err(io::Error::other)?.render())
                .map_err(io::Error::other)?;
        metadata
            .as_object_mut()
            .expect("serialized session object")
            .remove("messages");
        Ok(HistoryRecord::new(
            context.into(),
            Some(source_revision.into()),
            HistoryEvent::Projection { metadata, messages },
        ))
    }
}
