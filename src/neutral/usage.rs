//! Token accounting (spec 40 §2.4, 41 §2.4, 42 §2.3).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// `output_tokens_details.reasoning_tokens` / `thoughtsTokenCount`.
    pub reasoning_tokens: i64,
    /// `input_tokens_details.cached_tokens` / `cachedContentTokenCount`.
    pub cached_input_tokens: i64,
    /// Upstream total; 0 means "not reported".
    pub total_tokens: i64,
}

impl Usage {
    /// Fill `total_tokens` from input + output when the upstream did not report
    /// it (spec 40 §2.4).
    pub fn with_computed_total(mut self) -> Self {
        if self.total_tokens == 0 {
            self.total_tokens = self.input_tokens + self.output_tokens;
        }
        self
    }
}
