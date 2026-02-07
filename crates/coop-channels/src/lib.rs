#[cfg(feature = "signal")]
pub mod signal;
#[cfg(feature = "signal")]
pub mod signal_tools;
pub mod terminal;

#[cfg(feature = "signal")]
pub use signal::{SignalAction, SignalChannel, SignalTarget, SignalTypingNotifier};
#[cfg(feature = "signal")]
pub use signal_tools::SignalToolExecutor;
pub use terminal::TerminalChannel;
