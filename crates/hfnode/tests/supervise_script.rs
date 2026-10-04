//! `deploy/hfnode-supervise.sh`, the start-up script for macOS (and Unix without
//! systemd), against a stand-in for the hfnode binary: it must restart a failing
//! node and then give up, leave a clean stop alone, pass a stop signal on, kill a
//! node that ignores it, and after every exit put the radio on receive.
//!
//! Each case runs under every POSIX shell this machine has (`sh`, `dash`, `bash`).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/hfnode-supervise.sh"
);

/// Logs its arguments; `run` does what `$FAKE_RUN` says.
const FAKE: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_LOG"
case $1 in
run)
    echo "pw=${HFNODE_EMAIL_PASSWORD:-}" >> "$FAKE_LOG"
    case $FAKE_RUN in
    fail) exit 1 ;;
    clean) exit 0 ;;
    wait)
        trap 'echo TERM >> "$FAKE_LOG"; exit 0' TERM
        while :; do sleep 1; done ;;
    ignore)
        trap 'echo ignored >> "$FAKE_LOG"' TERM
        while :; do sleep 1; done ;;
    esac ;;
esac
exit 0
"#;

fn shells() -> Vec<&'static str> {
    ["sh", "dash", "bash"]
        .into_iter()
        .filter(|s| {
            Command::new(s)
                .args(["-c", "exit 0"])
                .status()
                .is_ok_and(|st| st.success())
        })
        .collect()
}

struct Case {
    _dir: tempfile::TempDir,
    log: PathBuf,
    fake: PathBuf,
}

impl Case {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("hfnode");
        fs::write(&fake, FAKE).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            log: dir.path().join("log"),
            fake,
            _dir: dir,
        }
    }

    fn start(&self, shell: &str, run: &str, env_file: Option<&Path>) -> Child {
        let mut cmd = Command::new(shell);
        cmd.arg(SCRIPT)
            .arg(&self.fake)
            .arg("node.toml")
            .env("FAKE_LOG", &self.log)
            .env("FAKE_RUN", run)
            .env("HFNODE_RESTART_SEC", "0")
            .env("HFNODE_STOP_TIMEOUT", "2")
            .env_remove("HFNODE_EMAIL_PASSWORD")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(f) = env_file {
            cmd.arg(f);
        }
        cmd.spawn().unwrap()
    }

    fn lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn count(&self, line: &str) -> usize {
        self.lines().iter().filter(|l| *l == line).count()
    }

    fn wait_for(&self, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.count(line) == 0 {
            assert!(
                Instant::now() < deadline,
                "no {line:?} in {:?}",
                self.lines()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn finish(child: &mut Child, within: Duration) -> ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the script did not finish within {within:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

const RUN: &str = "run --config node.toml";
const RX: &str = "radio --config node.toml rx";

#[test]
fn a_failing_node_is_restarted_then_given_up_on() {
    for sh in shells() {
        let c = Case::new();
        let status = finish(&mut c.start(sh, "fail", None), Duration::from_secs(20));
        assert_eq!(status.code(), Some(1), "{sh}");
        assert_eq!(c.count(RUN), 3, "{sh}: {:?}", c.lines());
        assert_eq!(c.count(RX), 3, "{sh}: {:?}", c.lines());
    }
}

#[test]
fn a_clean_stop_is_not_restarted() {
    for sh in shells() {
        let c = Case::new();
        let status = finish(&mut c.start(sh, "clean", None), Duration::from_secs(20));
        assert_eq!(status.code(), Some(0), "{sh}");
        assert_eq!(c.count(RUN), 1, "{sh}");
        assert_eq!(c.count(RX), 1, "{sh}");
    }
}

#[test]
fn a_stop_signal_is_passed_to_the_node() {
    for sh in shells() {
        let c = Case::new();
        let mut child = c.start(sh, "wait", None);
        c.wait_for(RUN);
        // Let the script reach its `wait`.
        thread::sleep(Duration::from_millis(300));
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        let status = finish(&mut child, Duration::from_secs(20));
        assert_eq!(status.code(), Some(0), "{sh}");
        let lines = c.lines();
        let term = lines.iter().position(|l| l == "TERM");
        let rx = lines.iter().position(|l| l == RX);
        assert!(term.is_some() && rx > term, "{sh}: {lines:?}");
        assert_eq!(c.count(RUN), 1, "{sh}");
    }
}

#[test]
fn a_node_that_ignores_the_stop_is_killed() {
    for sh in shells() {
        let c = Case::new();
        let mut child = c.start(sh, "ignore", None);
        c.wait_for(RUN);
        thread::sleep(Duration::from_millis(300));
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        let status = finish(&mut child, Duration::from_secs(20));
        assert_eq!(status.code(), Some(0), "{sh}");
        assert_eq!(c.count("ignored"), 1, "{sh}: {:?}", c.lines());
        assert_eq!(c.count(RX), 1, "{sh}: {:?}", c.lines());
    }
}

#[test]
fn secrets_come_from_the_env_file_without_running_it() {
    for sh in shells() {
        let c = Case::new();
        let env = c.log.with_file_name("env");
        let marker = c.log.with_file_name("ran");
        fs::write(
            &env,
            format!(
                "# node secrets\n\nHFNODE_EMAIL_PASSWORD=pa=ss word\n\
                 BAD-KEY=x\n$(touch {})\nLAST=no newline",
                marker.display()
            ),
        )
        .unwrap();
        let status = finish(
            &mut c.start(sh, "clean", Some(&env)),
            Duration::from_secs(20),
        );
        assert_eq!(status.code(), Some(0), "{sh}");
        assert_eq!(c.count("pw=pa=ss word"), 1, "{sh}: {:?}", c.lines());
        assert!(!marker.exists(), "{sh} ran a line of the env file");
    }
}
