use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often a child that has closed stdout is polled for its exit status.
const EXIT_POLL: Duration = Duration::from_millis(1);

/// Run a command and return its exit status and stdout, or `None` if it cannot
/// be started or has not finished within `timeout` (it is then killed).
///
/// Pass `program` as an absolute path: a bare name is looked up in the caller's
/// `PATH`, so the result would depend on the environment macpow is started from.
///
/// stderr is discarded; `Output::stderr` is always empty.
pub fn command_output_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        // Only stdout is piped. A pipe that nobody reads blocks the child as
        // soon as it holds 64 KiB, and an inherited stdin would let the child
        // read the user's keystrokes from under the TUI.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let output = collect_output(&mut child, Instant::now() + timeout);
    if output.is_none() {
        // Timed out or failed: leave neither a running child nor a zombie.
        let _ = child.kill();
        let _ = child.wait();
    }
    output
}

/// Read the child's stdout to its end, then collect the exit status. `None`
/// if either does not happen before `deadline`; the child is left as it is.
fn collect_output(child: &mut Child, deadline: Instant) -> Option<Output> {
    let mut pipe = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    // The pipe has to be drained while the child runs: reading it only after
    // the exit deadlocks on any output larger than the pipe buffer. A helper
    // thread does the blocking read so that this thread can keep the deadline.
    // After a timeout the helper ends when the pipe closes, which is when the
    // child is killed (or, if it left descendants holding the pipe, when the
    // last of them exits).
    std::thread::Builder::new()
        .name("child-stdout".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let read = pipe.read_to_end(&mut bytes).map(|_| bytes);
            let _ = tx.send(read);
        })
        .ok()?;
    // `None` when the deadline passes first, and when the read fails.
    let stdout = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()?;

    // End of stdout almost always means the child is exiting, but it may also
    // have closed stdout and carried on, so the wait stays bounded as well.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some(Output {
                    status,
                    stdout,
                    stderr: Vec::new(),
                })
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(EXIT_POLL),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generous: the tests only need it to be far above a healthy child's run
    /// time and far below "hangs forever".
    const TIMEOUT: Duration = Duration::from_secs(10);

    fn sh(script: &str, timeout: Duration) -> Option<Output> {
        command_output_timeout("/bin/sh", &["-c", script], timeout)
    }

    #[test]
    fn returns_stdout_and_exit_status() {
        let out = sh("printf hello", TIMEOUT).expect("sh runs");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");

        let out = sh("printf partial; exit 3", TIMEOUT).expect("sh runs");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"partial");
    }

    #[test]
    fn output_larger_than_the_pipe_buffer_is_returned_in_full() {
        // 1 MiB is 16 times the 64 KiB pipe buffer.
        let out = sh("/usr/bin/head -c 1048576 /dev/zero", TIMEOUT).expect("no deadlock");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 1_048_576);
    }

    #[test]
    fn stderr_is_discarded_without_blocking_the_child() {
        let out = sh("/usr/bin/head -c 200000 /dev/zero >&2; printf ok", TIMEOUT)
            .expect("stderr must not fill a pipe");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"ok");
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn stdin_is_not_inherited() {
        // `cat` copies stdin to stdout: with /dev/null it ends at once, with an
        // inherited terminal it would wait for input until the timeout.
        let out = command_output_timeout("/bin/cat", &[], TIMEOUT).expect("cat sees EOF");
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn child_that_outlives_the_timeout_is_killed() {
        let pid_file =
            std::env::temp_dir().join(format!("macpow-timeout-test-{}.pid", std::process::id()));
        let pid_path = pid_file.to_str().expect("utf-8 temp dir");
        let started = Instant::now();
        // `exec` keeps the sleeper in the process that was spawned (and whose
        // pid was written), so killing the child is killing the sleeper.
        let out = command_output_timeout(
            "/bin/sh",
            &["-c", "echo $$ > \"$0\"; exec /bin/sleep 60", pid_path],
            Duration::from_secs(1),
        );
        let elapsed = started.elapsed();
        let pid = std::fs::read_to_string(&pid_file);
        let _ = std::fs::remove_file(&pid_file);

        assert!(out.is_none());
        assert!(
            elapsed < Duration::from_secs(30),
            "returned after {elapsed:?}"
        );
        let pid: libc::pid_t = pid.expect("child started").trim().parse().expect("a pid");
        // SAFETY: signal 0 delivers nothing, it only reports whether `pid`
        // names a process. A killed and reaped child no longer does.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child {pid} is alive");
    }

    #[test]
    fn child_that_closes_stdout_and_lingers_still_times_out() {
        let started = Instant::now();
        let out = sh("exec >&-; exec /bin/sleep 60", Duration::from_millis(300));
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn missing_program_is_none() {
        assert!(command_output_timeout("/nonexistent/macpow-test-program", &[], TIMEOUT).is_none());
    }
}
