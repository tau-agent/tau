//! Integration test for the shutdown-hygiene fix (task #486).
//!
//! Spawns the `shutdown_harness` example binary, which spawns a long
//! `sleep` via the bash tool's tracked PGID registry.  Sends SIGTERM to
//! the harness and asserts that the bash process group goes away — i.e.
//! the harness's signal handler did call `kill_all_tracked()`.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn pgid_alive(pgid: i32) -> bool {
    // killpg(pgid, 0) probes existence without sending a signal.
    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None).is_ok()
}

fn wait_for_pgid_gone(pgid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pgid_alive(pgid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !pgid_alive(pgid)
}

#[test]
fn sigterm_kills_tracked_bash_children() {
    // Locate the harness binary that cargo just built.
    let exe = locate_harness();

    let mut child = Command::new(&exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn harness");

    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);

    // Read until READY, capturing the PGID line.
    let mut pgid: Option<i32> = None;
    let mut got_ready = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("PGID ")
                    && let Ok(p) = rest.parse::<i32>()
                {
                    pgid = Some(p);
                }
                if line == "READY" {
                    got_ready = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let pgid = pgid.expect("harness did not emit PGID");
    assert!(got_ready, "harness did not emit READY");
    assert!(
        pgid_alive(pgid),
        "harness reported pgid {} but it isn't alive?",
        pgid,
    );

    // Send SIGTERM to the harness.  Its installed handler should call
    // kill_all_tracked() then process::exit().
    let harness_pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(harness_pid, nix::sys::signal::Signal::SIGTERM).expect("kill harness");

    // The bash process group must go away.
    assert!(
        wait_for_pgid_gone(pgid, Duration::from_secs(5)),
        "bash pgid {} still alive 5s after SIGTERM to harness",
        pgid,
    );

    // The harness itself must exit.
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => panic!("wait harness: {}", e),
    };
    // 143 = 128 + SIGTERM(15)
    assert_eq!(
        status.code(),
        Some(143),
        "harness exited with unexpected status: {:?}",
        status,
    );
}

#[test]
fn sighup_kills_tracked_bash_children() {
    // Same as the SIGTERM test but with SIGHUP (parent shell closes).
    let exe = locate_harness();

    let mut child = Command::new(&exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn harness");

    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);

    let mut pgid: Option<i32> = None;
    let mut got_ready = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("PGID ")
                    && let Ok(p) = rest.parse::<i32>()
                {
                    pgid = Some(p);
                }
                if line == "READY" {
                    got_ready = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let pgid = pgid.expect("harness did not emit PGID");
    assert!(got_ready, "harness did not emit READY");

    let harness_pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(harness_pid, nix::sys::signal::Signal::SIGHUP).expect("kill harness");

    assert!(
        wait_for_pgid_gone(pgid, Duration::from_secs(5)),
        "bash pgid {} still alive 5s after SIGHUP to harness",
        pgid,
    );

    let status = child.wait().expect("wait harness");
    // 129 = 128 + SIGHUP(1)
    assert_eq!(
        status.code(),
        Some(129),
        "harness exited with unexpected status: {:?}",
        status,
    );
}

fn locate_harness() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_shutdown_harness") {
        return std::path::PathBuf::from(p);
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "--example", "shutdown_harness", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "failed to build shutdown_harness");
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target");
    p.push("debug");
    p.push("examples");
    p.push("shutdown_harness");
    p
}
