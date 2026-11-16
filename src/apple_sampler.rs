use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub fn start() -> Option<AppleSampler> {
    eprintln!("macpow is starting Apple's power sampler for individual CPU wattage.");
    let mut command = Command::new("/usr/bin/sudo");
    command.args([
        "-p",
        "[macpow] Mac password for per-core CPU power: ",
        "--",
        "/usr/bin/powermetrics",
        "--samplers",
        "cpu_power",
        "-i",
        "250",
        "--buffer-size",
        "0",
    ]);

    match AppleSampler::launch(command) {
        Ok(sampler) => Some(sampler),
        Err(_) => {
            eprintln!(
                "Apple's power sampler could not start. Individual core power is unavailable."
            );
            None
        }
    }
}

pub struct AppleSampler {
    child: Child,
    running: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl AppleSampler {
    fn launch(mut command: Command) -> io::Result<Self> {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut sampler = Self {
            child,
            running: Arc::new(AtomicBool::new(true)),
            reader: None,
        };
        let mut output = sampler
            .child
            .stdout
            .take()
            .expect("sampler output is piped");
        // sudo reads its password from the controlling terminal. Wait for
        // powermetrics output before the TUI takes over terminal input.
        output.read_exact(&mut [0])?;
        let running = sampler.running.clone();
        sampler.reader = Some(thread::Builder::new().spawn(move || drain(output, running))?);
        Ok(sampler)
    }
}

fn drain(mut output: ChildStdout, running: Arc<AtomicBool>) {
    let mut fd = libc::pollfd {
        fd: output.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut buf = [0; 8192];
    while running.load(Ordering::Relaxed) {
        match unsafe { libc::poll(&mut fd, 1, 100) } {
            0 => continue,
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
            -1 => break,
            _ => match output.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(_) => break,
            },
        }
    }
}

impl Drop for AppleSampler {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // Closing the output pipe stops powermetrics on its next write, even
        // if macpow cannot signal the root process. The OS also closes the
        // pipe if macpow is terminated without running this destructor.
        let deadline = Instant::now() + Duration::from_secs(1);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_the_session_stops_a_streaming_sampler() {
        let sampler = AppleSampler::launch(Command::new("/usr/bin/yes")).unwrap();
        let pid = sampler.child.id() as libc::pid_t;
        drop(sampler);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn failure_before_the_first_sample_returns_control_to_the_ui() {
        let result = AppleSampler::launch(Command::new("/usr/bin/false"));
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::UnexpectedEof);
    }
}
