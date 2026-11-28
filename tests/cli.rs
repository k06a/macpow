//! End-to-end checks of the command-line contract: how the real binary parses
//! its arguments, what it writes and how it ends. The `--json` and TUI tests
//! start the samplers, so they need a Mac; every wait is bounded, so a
//! regression fails the test instead of hanging it.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
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

/// Reads the first line of the child's stdout and then closes the pipe, like
/// `head -1` does. Kills the child and fails if no line arrives within `LIMIT`.
fn first_line_then_close(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
        // The reader is dropped here, which closes our end of the pipe.
    });
    match rx.recv_timeout(LIMIT) {
        Ok(line) => line,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("macpow printed no line within {LIMIT:?}");
        }
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

/// Unvalidated, `--interval 0` made `--json` write duplicates as fast as the
/// pipe took them, while the samplers silently ran at 100 ms.
#[test]
fn interval_outside_100_to_60000_ms_is_rejected() {
    for value in ["0", "99", "60001"] {
        // No --json and no terminal: even without the validation this run
        // could not get as far as producing output.
        let mut child = spawn(&["--interval", value]);
        let status = wait(&mut child);
        let stderr = stderr_of(&mut child);
        assert_eq!(status.code(), Some(2), "--interval {value}: {stderr}");
        assert!(
            stderr.contains("100..=60000"),
            "--interval {value}: {stderr}"
        );
    }
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

/// `macpow --json | head -1`: the reader gets one whole object, and macpow
/// ends quietly when its next write finds the pipe closed. It used to print
/// the first line of a pretty-printed object and then panic (exit 101).
#[test]
fn json_is_one_object_per_line_and_a_closed_pipe_ends_cleanly() {
    let mut child = spawn(&["--json", "--interval", "100"]);
    let line = first_line_then_close(&mut child);
    let status = wait(&mut child);
    let stderr = stderr_of(&mut child);

    assert!(line.ends_with('\n'), "first line: {line:?}");
    let value: serde_json::Value =
        serde_json::from_str(&line).expect("the first line is not one complete JSON value");
    assert!(value.is_object(), "first line: {line:?}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert_eq!(status.code(), Some(0), "status: {status:?}");
}

const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const MOUSE_REPORTING_OFF: &[u8] = b"\x1b[?1000l";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The TUI running on a pseudo-terminal, with everything it writes recorded.
struct Tui {
    child: Child,
    /// Our end of the pseudo-terminal; writing to it types into the TUI.
    keyboard: File,
    screen: Arc<Mutex<Vec<u8>>>,
}

impl Tui {
    fn start() -> Self {
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: 50,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty(3) stores two new descriptors in the out-parameters
        // and reads the window size; it accepts null for the name and termios.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: both descriptors were just created and have no other owner.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };

        // The Command is a temporary, so our copies of the terminal side are
        // closed as soon as the child has them.
        let child = Command::new(BIN)
            .stdin(slave.try_clone().expect("dup"))
            .stdout(slave.try_clone().expect("dup"))
            .stderr(slave)
            .spawn()
            .expect("failed to start macpow");

        let screen = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::clone(&screen);
        let mut output = master.try_clone().expect("dup");
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            // Ends with EOF or an error once macpow has closed its side.
            while let Ok(n @ 1..) = output.read(&mut chunk) {
                recording.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        Tui {
            child,
            keyboard: master,
            screen,
        }
    }

    /// Polls the recording until `done` accepts it; fails after `LIMIT`.
    fn wait_until(&self, what: &str, done: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + LIMIT;
        while !done(&self.screen.lock().unwrap()) {
            assert!(Instant::now() < deadline, "the TUI never {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_first_frame(&self) {
        self.wait_until("drew its footer", |screen| contains(screen, b"quit"));
    }

    /// The terminal must end up the way a shell needs it: the very last thing
    /// written is "leave the alternate screen", after mouse reporting is off.
    fn assert_terminal_restored(&self) {
        self.wait_until("left the alternate screen", |screen| {
            screen.ends_with(LEAVE_ALTERNATE_SCREEN)
        });
        assert!(contains(&self.screen.lock().unwrap(), MOUSE_REPORTING_OFF));
    }
}

impl Drop for Tui {
    /// A failed assertion must not leave a TUI process behind.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// SIGTERM (`kill`, a supervisor, `brew upgrade`) used to end the TUI on the
/// spot and leave the shell in raw mode on the alternate screen.
#[test]
fn tui_sigterm_restores_the_terminal() {
    let mut tui = Tui::start();
    tui.wait_for_first_frame();
    // SAFETY: kill(2) on the PID of a child we spawned and have not reaped.
    unsafe { libc::kill(tui.child.id() as libc::pid_t, libc::SIGTERM) };

    let status = wait(&mut tui.child);
    assert_eq!(status.code(), Some(0), "status: {status:?}");
    tui.assert_terminal_restored();
}

#[test]
fn tui_quit_key_restores_the_terminal_and_exits_0() {
    let mut tui = Tui::start();
    tui.wait_for_first_frame();
    tui.keyboard.write_all(b"q").expect("typing into the TUI");

    let status = wait(&mut tui.child);
    assert_eq!(status.code(), Some(0), "status: {status:?}");
    tui.assert_terminal_restored();
}
