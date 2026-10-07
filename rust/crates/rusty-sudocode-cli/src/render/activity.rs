//! Foreground activity projection. Engine lifecycle owns busy/input/idle;
//! animation and temporary output suspension never change that lifecycle.

use std::{
    fmt::Write,
    sync::{Arc, Mutex},
    time::Instant,
};

use engine_events::EngineState;

use super::{ansi_fg, spinner_progress::SpinnerProgress, theme, RESET};

pub(crate) const WORKING_LABEL: &str = "🦀 Working...";
pub(super) const REASONING_LABEL: &str = "🧠 Reasoning...";

#[derive(Clone)]
enum ActivityPhase {
    Working,
    Reasoning,
    Retry {
        attempt: u32,
        max_retries: u32,
        reason: String,
    },
}

struct TurnMetadata {
    start: Instant,
    model: Option<String>,
    token_budget: Option<u32>,
}

/// Shared renderer projection fed by the session's ordered engine event bridge.
#[derive(Clone)]
pub(crate) struct ActivityState {
    pub(crate) progress: Arc<SpinnerProgress>,
    lifecycle: Arc<Mutex<EngineState>>,
    phase: Arc<Mutex<ActivityPhase>>,
    metadata: Arc<Mutex<TurnMetadata>>,
}

impl ActivityState {
    pub(crate) fn new() -> Self {
        Self {
            progress: Arc::new(SpinnerProgress::new()),
            lifecycle: Arc::new(Mutex::new(EngineState::Idle)),
            phase: Arc::new(Mutex::new(ActivityPhase::Working)),
            metadata: Arc::new(Mutex::new(TurnMetadata {
                start: Instant::now(),
                model: None,
                token_budget: None,
            })),
        }
    }

    /// Prepare counters before submission; only engine events activate a turn.
    pub(crate) fn prepare_turn(&self, model: &str, token_budget: Option<u32>) {
        self.progress.reset();
        *self.phase.lock().unwrap() = ActivityPhase::Working;
        *self.metadata.lock().unwrap() = TurnMetadata {
            start: Instant::now(),
            model: Some(model.to_string()),
            token_budget,
        };
    }

    #[inline]
    pub(crate) fn engine_state(&self) -> EngineState {
        *self.lifecycle.lock().unwrap()
    }

    /// Called only by the event bridge, including while the renderer is idle.
    #[inline]
    pub(crate) fn observe_engine_state(&self, state: EngineState) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if *lifecycle != state {
            *lifecycle = state;
            self.progress.record_activity();
        }
    }

    pub(crate) fn set_reasoning(&self, on: bool) {
        *self.phase.lock().unwrap() = if on {
            ActivityPhase::Reasoning
        } else {
            ActivityPhase::Working
        };
    }

    pub(crate) fn set_retry(&self, attempt: u32, max_retries: u32, reason: &str) {
        *self.phase.lock().unwrap() = ActivityPhase::Retry {
            attempt,
            max_retries,
            reason: reason.to_string(),
        };
    }

    /// Render a frame for one lifecycle snapshot, shared by status and footer.
    pub(crate) fn render_frame(&self, frame_index: usize, state: EngineState) -> String {
        match state {
            EngineState::Idle => return String::new(),
            EngineState::AwaitingInput => {
                return format!(
                    "{}? Waiting for your input{RESET}",
                    ansi_fg(theme().warning)
                )
            }
            EngineState::Running => {}
        }
        let phase = self.phase.lock().unwrap().clone();
        let reasoning = matches!(phase, ActivityPhase::Reasoning);
        let retry = matches!(phase, ActivityPhase::Retry { .. });
        let retry_label;
        let (frame, label) = match phase {
            ActivityPhase::Retry {
                attempt,
                max_retries,
                reason,
            } => {
                retry_label = format!("Retry {attempt}/{max_retries}: {reason}");
                ("⟳", retry_label.as_str())
            }
            ActivityPhase::Reasoning => (animation_frame(frame_index, true), REASONING_LABEL),
            ActivityPhase::Working => (animation_frame(frame_index, false), WORKING_LABEL),
        };
        let metadata = self.metadata.lock().unwrap();
        let line = progress_line(
            frame,
            label,
            metadata.model.as_deref(),
            metadata.start.elapsed().as_secs_f64(),
            self.progress.response_bytes(),
            metadata.token_budget,
        );
        let color = theme().spinner_fg(retry || self.progress.is_stalled(reasoning));
        format!("{color}{line}{RESET}")
    }
}

#[inline]
pub(super) fn animation_frame(index: usize, reasoning: bool) -> &'static str {
    let frames: &[&str] = if reasoning {
        &["◐", "◓", "◑", "◒"]
    } else {
        &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
    };
    frames[index % frames.len()]
}

/// One formatter for inline and one-shot model/time/token/budget/ETA display.
#[inline]
pub(super) fn progress_line(
    frame: &str,
    label: &str,
    model: Option<&str>,
    elapsed: f64,
    bytes: u32,
    token_budget: Option<u32>,
) -> String {
    let mut line = format!("{frame} {label}");
    if let Some(model) = model {
        let _ = write!(line, " [{model}]");
    }
    let _ = write!(line, " ({elapsed:.1}s)");
    if bytes > 0 && elapsed >= 1.0 {
        let tokens = bytes / 4;
        if let Some(budget) = token_budget {
            let pct = (f64::from(tokens) / f64::from(budget) * 100.0).min(100.0);
            let _ = write!(
                line,
                " ↓ {} / {} ({pct:.0}%)",
                compact_tokens(tokens),
                compact_tokens(budget)
            );
            if tokens >= 2000 && elapsed > 5.0 {
                let rate = f64::from(tokens) / elapsed;
                let eta = f64::from(budget.saturating_sub(tokens)) / rate;
                let _ = if eta >= 60.0 {
                    write!(line, " ~{:.0}m", eta / 60.0)
                } else {
                    write!(line, " ~{eta:.0}s")
                };
            }
        } else if tokens >= 1000 {
            let _ = write!(line, " ↓ {:.1}k tokens", f64::from(tokens) / 1000.0);
        } else {
            let _ = write!(line, " ↓ {tokens} tokens");
        }
    }
    line
}

fn compact_tokens(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", f64::from(tokens) / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", f64::from(tokens) / 1_000.0)
    } else {
        tokens.to_string()
    }
}
