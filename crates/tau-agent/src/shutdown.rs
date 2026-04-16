//! Cross-process signal-driven shutdown helper.
//!
//! All long-running tau processes (cli, server, worker subprocess) need
//! to react to SIGTERM / SIGHUP (and sometimes SIGINT) so that:
//!
//! - in-flight agent turns are cancelled before the process exits;
//! - sessions are flushed / phases reset cleanly;
//! - bash child process groups spawned by the worker are not orphaned
//!   (see [`tau_agent_plugin_worker::tools::bash::kill_all_tracked`]).
//!
//! The mechanism here is a "signal handler thread": we block the relevant
//! signals process-wide using `pthread_sigmask`, then spawn a dedicated
//! OS thread that loops over `sigwait()`.  This is race-free — the signal
//! handler thread runs *normal* Rust code, never a real OS signal handler,
//! so we can hold mutexes, allocate, etc.
//!
//! Callers pass a closure that is invoked once per signal received.  It is
//! the caller's responsibility to translate that into a graceful shutdown
//! (cancel chats, set a flag, drop a runtime, …).
//!
//! `install_*` must be called **before any other thread is spawned** so
//! that the inherited signal mask actually takes effect.  In practice
//! this means "as the first thing in `main`".
//!
//! Within a single process the waiter thread is shared: subsequent
//! callers use [`set_handler`] to swap the callback atomically.  This is
//! what the CLI does when it transitions between modes (TUI ↔ non-TUI).

use std::sync::{Mutex, OnceLock};

use nix::sys::signal::{SigSet, Signal};

type Handler = Box<dyn Fn(Signal) + Send + Sync + 'static>;

static HANDLER: OnceLock<Mutex<Handler>> = OnceLock::new();

/// Set of signals this module handles by default.
///
/// SIGTERM / SIGHUP are always handled.  SIGINT is included so that, in
/// non-TUI contexts, Ctrl-C also triggers the same graceful path.  Inside
/// the TUI raw mode, callers should use [`install_for`] with `[SIGTERM,
/// SIGHUP]` only and let SIGINT keep reaching the terminal-event loop.
fn default_signals() -> [Signal; 3] {
    [Signal::SIGTERM, Signal::SIGHUP, Signal::SIGINT]
}

/// Replace the current signal callback.
///
/// Safe to call at any time after [`install`] / [`install_for`] has been
/// called.  The swap is atomic — a signal delivered during replacement
/// either runs the old handler or the new one, never neither.
///
/// If no waiter has been installed yet this falls back to a fresh
/// [`install`] call (which masks the default signal set).
pub fn set_handler<F>(on_signal: F)
where
    F: Fn(Signal) + Send + Sync + 'static,
{
    if let Some(slot) = HANDLER.get() {
        *slot.lock().expect("signal handler mutex poisoned") = Box::new(on_signal);
    } else if let Err(e) = install(on_signal) {
        eprintln!("set_handler: install failed: {}", e);
    }
}

/// Install a signal-handler thread for the given signal set.
///
/// Blocks the listed signals process-wide and (on the first call) spawns
/// a thread that calls `on_signal` for each one received.  Subsequent
/// calls replace the handler callback in place without spawning another
/// waiter thread.
///
/// `on_signal` may run arbitrary Rust code (locks, allocation, channels):
/// it is **not** invoked from a signal handler context.
///
/// Returns `Ok(())` on success.  Failures are non-fatal in practice (the
/// process will still run, just without graceful signal handling), so
/// callers typically log and continue.
///
/// **Must be called before any other thread is spawned.**  The signal
/// mask set here is only inherited by *subsequently* spawned threads;
/// any thread alive when this runs will retain its old mask and the
/// kernel may deliver signals to it instead of the waiter thread.
pub fn install_for<I, F>(signals: I, on_signal: F) -> nix::Result<()>
where
    I: IntoIterator<Item = Signal>,
    F: Fn(Signal) + Send + Sync + 'static,
{
    let mut mask = SigSet::empty();
    for sig in signals {
        mask.add(sig);
    }

    // Block in *this* thread; the mask is inherited by threads spawned
    // afterwards.  This is why `install_for` must be called early in main.
    mask.thread_block()?;

    let first_time = HANDLER.get().is_none();
    if first_time {
        // Initialise the shared slot with the provided callback and
        // spawn the single long-lived waiter thread.
        let slot: &'static Mutex<Handler> = HANDLER.get_or_init(|| Mutex::new(Box::new(on_signal)));
        std::thread::Builder::new()
            .name("tau-signal-waiter".to_string())
            .spawn(move || {
                loop {
                    match mask.wait() {
                        Ok(sig) => {
                            let handler = slot.lock().expect("signal handler mutex poisoned");
                            handler(sig);
                        }
                        Err(e) => {
                            if e == nix::errno::Errno::EINTR {
                                continue;
                            }
                            eprintln!("signal-waiter thread: sigwait failed: {}", e);
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
            })
            .map_err(|e| {
                eprintln!("failed to spawn signal-waiter thread: {}", e);
                nix::errno::Errno::EAGAIN
            })?;
    } else {
        // Waiter already running — swap in the new callback.
        let slot = HANDLER.get().expect("HANDLER should be initialized");
        *slot.lock().expect("signal handler mutex poisoned") = Box::new(on_signal);
    }

    Ok(())
}

/// Install a signal-handler thread for SIGTERM, SIGHUP, and SIGINT.
///
/// See [`install_for`] for full details.  Suitable for headless / non-TUI
/// processes (server, worker, `tau --no-tui`).  TUI callers should use
/// [`install_for`] with `[SIGTERM, SIGHUP]` only and let SIGINT keep
/// reaching the terminal-event loop.
pub fn install<F>(on_signal: F) -> nix::Result<()>
where
    F: Fn(Signal) + Send + Sync + 'static,
{
    install_for(default_signals(), on_signal)
}

/// Convenience: short human label for a signal.
pub fn signal_name(sig: Signal) -> &'static str {
    match sig {
        Signal::SIGTERM => "SIGTERM",
        Signal::SIGHUP => "SIGHUP",
        Signal::SIGINT => "SIGINT",
        _ => "signal",
    }
}
