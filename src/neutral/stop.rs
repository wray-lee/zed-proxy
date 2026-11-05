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
    /// Upstream stop-sequence hit. Kept for the neutral set (spec 45 §1.2);
    /// every provider currently folds `stop_sequence` into `Stop`.
    #[allow(dead_code)]
    StopSequence,
    Unknown,
}
