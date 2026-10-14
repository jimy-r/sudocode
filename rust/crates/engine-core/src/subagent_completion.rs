//! The engine event adapter for the shared completion qualification reducer.
use crate::EngineEvent;
use engine_events::{SubagentLifecycle, SubagentUpdate};

#[derive(Default)]
pub struct Completions(runtime::subagent_completion::Completions);

impl Completions {
    pub fn observe(&mut self, event: &EngineEvent) -> Vec<SubagentLifecycle> {
        match event {
            EngineEvent::TurnStarted { .. } => self.0.begin(),
            EngineEvent::TurnComplete(_) | EngineEvent::Error { .. } => self.0.close(),
            EngineEvent::Subagent(event) if event.stream.is_none() => {
                if let SubagentUpdate::Lifecycle(lifecycle) = &event.update {
                    self.0.lifecycle(lifecycle);
                }
            }
            EngineEvent::ToolResult {
                id, name, output, ..
            } => self.0.tool_result(id, name, output),
            _ => return Vec::new(),
        }
        self.0.drain()
    }
}

pub use tools::recover_subagent_completion as recover;
