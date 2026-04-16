//! Test harness used by `tests/shutdown_kills_bash.rs`.
//!
//! Spawns a long-running bash via the same tracked-PGID code path the
//! real worker uses, prints the bash PGID on stdout, then sleeps until
//! killed.  The test sends SIGTERM and asserts the PGID is gone.
//!
//! Build & invoked via `cargo run --example shutdown_harness`.

fn main() {
    // Install signal handlers — same pattern as `tau worker`.
    if let Err(e) = tau_agent::shutdown::install(|sig| {
        eprintln!(
            "harness: received {}, killing tracked bash children",
            tau_agent::shutdown::signal_name(sig),
        );
        tau_agent_plugin_worker::tools::bash::kill_all_tracked();
        let code = match sig {
            nix::sys::signal::Signal::SIGTERM => 143,
            nix::sys::signal::Signal::SIGHUP => 129,
            nix::sys::signal::Signal::SIGINT => 130,
            _ => 1,
        };
        std::process::exit(code);
    }) {
        eprintln!("install failed: {}", e);
        std::process::exit(2);
    }

    // Spawn a sleep on a background thread (so the registry is populated
    // before main reports readiness).  We use the bash tool's normal
    // execute() entry point so we exercise the production track/untrack
    // code path.
    std::thread::spawn(|| {
        let _ = tau_agent_plugin_worker::tools::bash::execute_streaming(
            &serde_json::json!({"command": "sleep 600", "timeout": 1200}),
            "/tmp",
            |_| {},
        );
    });

    // Wait for the registry to populate, then print the PGID and "READY"
    // so the test driver can capture it.
    use std::io::Write;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Some(pgid) = tau_agent_plugin_worker::tools::bash::first_tracked_pgid() {
            println!("PGID {}", pgid);
            println!("READY");
            std::io::stdout().flush().ok();
            break;
        }
    }

    // Sleep until killed.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
