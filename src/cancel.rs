//! Global cancellation flag.
//!
//! A single run of psync is one process, so one process-wide flag is all we
//! need. SIGINT and SIGTERM set it (via `signal-hook`'s async-signal-clean
//! `flag::register`); every worker checks it between chunks and between
//! jobs. Workers also handle `Err(AppError::Cancelled)` by cleaning up
//! their partial work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

static CANCEL_ARC: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The process-wide cancellation flag (created on first use).
fn cancel_flag() -> &'static Arc<AtomicBool> {
    CANCEL_ARC.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// Register SIGINT/SIGTERM handlers that set the global cancellation flag.
/// Safe to call once; repeated registrations are handled by signal-hook
/// (each call registers a new handler, which is fine here).
pub fn install_handlers() {
    for sig in [
        signal_hook::consts::signal::SIGINT,
        signal_hook::consts::signal::SIGTERM,
    ] {
        if let Err(e) = signal_hook::flag::register(sig, cancel_flag().clone()) {
            eprintln!("psync: warning: could not install signal handler for signal {sig}: {e}");
        }
    }
}

/// True once the user asked us to stop.
pub fn is_cancelled() -> bool {
    cancel_flag().load(Ordering::Relaxed)
}

/// Wait up to `timeout` for cancellation, polling. Returns true if cancelled.
pub fn wait_for_cancel(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if is_cancelled() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    is_cancelled()
}
