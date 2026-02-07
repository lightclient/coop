#[cfg(feature = "signal")]
pub mod signal;
pub mod terminal;

#[cfg(feature = "signal")]
pub use signal::{SignalChannel, SignalTarget};
pub use terminal::TerminalChannel;
