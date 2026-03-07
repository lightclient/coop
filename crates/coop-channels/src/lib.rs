#[cfg(feature = "signal")]
pub mod signal;
#[cfg(feature = "signal")]
pub mod signal_tools;

#[cfg(feature = "signal")]
pub use signal::{
    MockSignalChannel, SignalAction, SignalChannel, SignalQuery, SignalTarget, SignalTypingNotifier,
};
#[cfg(feature = "signal")]
pub use signal_tools::SignalToolExecutor;
