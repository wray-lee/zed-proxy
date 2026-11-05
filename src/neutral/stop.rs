//! Neutral finish reasons (spec 45 §1.2 `end`).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    Stop,
    /// Output budget exhausted (`length` / `max_tokens` / `MAX_TOKENS`).
    Length,
    ToolCalls,
    /// Refusal / safety stop (`content_filter` / `refusal` / `SAFETY`).
    ContentFilter,
    StopSequence,
    Unknown,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolCalls => "tool_calls",
            StopReason::ContentFilter => "content_filter",
            StopReason::StopSequence => "stop_sequence",
            StopReason::Unknown => "unknown",
        }
    }
}
