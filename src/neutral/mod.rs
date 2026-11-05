//! Neutral (client-protocol and provider independent) data types.
//!
//! Spec: `45-conversion-matrix.md` §1. Client protocol decoders produce
//! [`NeutralRequest`]; provider encoders consume it and emit [`StreamEvent`]s or
//! a [`NeutralResponse`]; client protocol encoders turn those back into the
//! client's JSON / SSE.

pub mod content;
pub mod event;
pub mod request;
pub mod response;
pub mod stop;
pub mod tools;
pub mod usage;

pub use content::{
    ContentBlock, ImageSource, Message, Opaque, Role, SystemBlock, ToolResult, ToolUse,
};
pub use event::{BlockKind, StreamEvent};
pub use request::{NeutralRequest, Reasoning};
pub use response::NeutralResponse;
pub use stop::StopReason;
pub use tools::{ToolChoice, ToolDef};
pub use usage::Usage;
