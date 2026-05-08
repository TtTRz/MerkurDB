pub mod llm;
pub mod noop;

pub use llm::{LlmBackend, LlmConsolidator};
pub use noop::NoopConsolidator;
