//! Error helpers for the tasks plugin.
//!
//! The plugin's fallible code used to repeat this pattern ~100 times:
//!
//! ```ignore
//! something().map_err(|e| tau_agent_plugin::Error::Io(format!("ctx: {}", e)))?;
//! ```
//!
//! [`plugin_io_err`] returns a closure suitable for `map_err` that builds
//! the same `"ctx: <err>"` message, so each call site shrinks to:
//!
//! ```ignore
//! something().map_err(plugin_io_err("ctx"))?;
//! ```
//!
//! This also nails down the separator (`": "`) so error messages are
//! uniform across the crate.

use std::fmt::Display;

/// Build a `map_err` closure that wraps any `Display`able error into a
/// [`tau_agent_plugin::Error::Io`] with the message `"<ctx>: <err>"`.
///
/// Prefer this over hand-rolled `|e| tau_agent_plugin::Error::Io(format!(...))`
/// closures so the error-message convention stays uniform.
pub(crate) fn plugin_io_err<E: Display>(
    ctx: &'static str,
) -> impl FnOnce(E) -> tau_agent_plugin::Error {
    move |e| tau_agent_plugin::Error::Io(format!("{}: {}", ctx, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ctx_and_error_with_colon_separator() {
        let inner = std::io::Error::other("boom");
        let mapped = plugin_io_err("open file")(inner);
        match mapped {
            tau_agent_plugin::Error::Io(msg) => assert_eq!(msg, "open file: boom"),
            other => panic!("expected Error::Io, got {:?}", other),
        }
    }

    #[test]
    fn works_with_display_types() {
        // Any Display works, not just io::Error.
        let mapped = plugin_io_err("ctx")("bang");
        match mapped {
            tau_agent_plugin::Error::Io(msg) => assert_eq!(msg, "ctx: bang"),
            other => panic!("expected Error::Io, got {:?}", other),
        }
    }

    #[test]
    fn usable_via_map_err() {
        let r: Result<(), _> = Err::<(), _>("nope").map_err(plugin_io_err("wrap"));
        match r {
            Err(tau_agent_plugin::Error::Io(msg)) => assert_eq!(msg, "wrap: nope"),
            other => panic!("expected Err(Io), got {:?}", other),
        }
    }
}
