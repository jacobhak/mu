//! The signals a session answers. The order is fixed (see `run::run`): a handler is installed, then
//! the session record is told it exists, then the agent task starts.

use mu_base::events::Command;
use mu_session::session::owner;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// `-p` and plan runs: SIGUSR1 stops the run at the next boundary, as `mu take` asks. Installed before
/// the agent task can create or lock a session, so the owner record can promise that SIGUSR1 stops the run.
pub fn print_run_stop() -> std::io::Result<UnboundedReceiver<()>> {
    let mut sig = signal(SignalKind::user_defined1())?;
    let (stop_tx, stop_rx) = unbounded_channel::<()>();
    owner::set_print_run();
    tokio::task::spawn_local(async move {
        while sig.recv().await.is_some() {
            if stop_tx.send(()).is_err() {
                break;
            }
        }
    });
    Ok(stop_rx)
}

/// Interactive: SIGUSR1 cancels the running turn, like Esc (the jobs panel's `s` on another session).
pub fn cancel_turn_on_sigusr1(cmd: UnboundedSender<Command>) {
    match signal(SignalKind::user_defined1()) {
        Ok(mut sig) => {
            tokio::task::spawn_local(async move {
                while sig.recv().await.is_some() {
                    let _ = cmd.send(Command::Cancel);
                }
            });
            owner::set_abort_signal();
        }
        Err(e) => {
            eprintln!("mu: cannot install the SIGUSR1 handler ({e}); other sessions cannot abort this one")
        }
    }
}
