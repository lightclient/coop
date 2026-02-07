#[cfg(feature = "signal")]
pub mod signal;
pub mod terminal;

#[cfg(feature = "signal")]
pub use signal::{SignalChannel, SignalHandle, signal_pair};
pub use terminal::TerminalChannel;
