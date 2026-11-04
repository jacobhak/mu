//! The tokio runtime the one-shot CLI paths run on. Callers that spawn `!Send` tasks wrap it in a
//! `tokio::task::LocalSet`.

/// A current-thread runtime with timers and IO enabled.
pub fn current_thread_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}
