//! A stop signal reaches hfnode's own handler (which, with a radio in use, puts it
//! back on receive before exiting) instead of killing the process outright. With no
//! radio in use the handler exits 130, as an interrupted program would.
//!
//! Unix only: there is no portable way for a test to send a console Ctrl-C to one
//! child process on Windows.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn stopped_by(signal: &str) -> Option<i32> {
    // The self-test at real time runs for minutes; it is stopped long before that.
    let mut child = Command::new(env!("CARGO_BIN_EXE_hfnode"))
        .args(["selftest", "--scale", "1", "--jobs", "1"])
        .env("RUST_LOG", "hfnode::signal=debug")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Signal only once the handler is in place, however slow the machine.
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let (ready_tx, ready) = mpsc::channel();
    thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if line.contains("stop-signal handler installed") {
                let _ = ready_tx.send(());
            }
        }
    });
    if ready.recv_timeout(Duration::from_secs(60)).is_err() {
        let _ = child.kill();
        panic!("hfnode did not report its stop-signal handler");
    }
    let sent = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("hfnode did not stop on SIG{signal}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stop_signals_reach_the_handler() {
    // Killed by the signal itself, the exit code would be None.
    for signal in ["TERM", "INT", "HUP"] {
        assert_eq!(stopped_by(signal), Some(130), "SIG{signal}");
    }
}
