//! End-to-end checks of the command-line contract: how the real binary parses
//! its arguments, what it writes and how it ends. The `--json` tests start the
//! samplers, so they need a Mac; every wait is bounded, so a regression fails
//! the test instead of hanging it.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_macpow");

/// Generous, because other builds may share the machine: start-up alone can
/// take over a second under load.
const LIMIT: Duration = Duration::from_secs(60);

/// Starts `macpow` with both output streams captured through pipes.
fn spawn(args: &[&str]) -> Child {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A shell that runs the test suite as a background job starts it with
    // SIGINT ignored, and children inherit that. Give macpow the disposition
    // it has when a user presses Ctrl+C.
    // SAFETY: the closure runs between fork and exec and only calls
    // signal(2), which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    cmd.spawn().expect("failed to start macpow")
}

/// Waits for the child to exit. Kills it and fails if it outlives `LIMIT`.
fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("macpow was still running after {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Everything the (already exited) child wrote to stderr.
fn stderr_of(child: &mut Child) -> String {
    let mut text = String::new();
    child
        .stderr
        .take()
        .expect("stderr is piped")
        .read_to_string(&mut text)
        .expect("stderr is not UTF-8");
    text
}

#[test]
fn version_prints_the_package_version() {
    let out = Command::new(BIN)
        .arg("--version")
        .output()
        .expect("failed to start macpow");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        concat!("macpow ", env!("CARGO_PKG_VERSION"))
    );
}

/// Ctrl+C while the reader is not keeping up used to run `process::exit` in
/// the signal handler and abort with "RefCell already borrowed" (exit 134).
#[test]
fn json_sigint_with_a_stalled_reader_ends_by_the_signal() {
    let mut child = spawn(&["--json", "--interval", "100"]);
    // Nobody reads stdout, so the pipe fills up and macpow blocks in write().
    std::thread::sleep(Duration::from_millis(2500));
    // SAFETY: kill(2) on the PID of a child we spawned and have not reaped.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };

    let status = wait(&mut child);
    let stderr = stderr_of(&mut child);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert_eq!(status.signal(), Some(libc::SIGINT), "status: {status:?}");
}
