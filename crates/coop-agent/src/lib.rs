mod goose_subprocess;
#[cfg(test)]
mod smoke_test;

// Re-export Goose's provider layer — this is the core integration point.
// See docs/goose-integration.md for the design decision.
pub use ::goose::providers::base::{Provider, ProviderUsage, Usage, ModelInfo};
pub use ::goose::providers::{create_with_named_model};
pub use ::goose::conversation::message::Message as GooseMessage;
pub use ::goose::token_counter;

// Keep the subprocess runtime available as a fallback
pub use goose_subprocess::GooseRuntime;
